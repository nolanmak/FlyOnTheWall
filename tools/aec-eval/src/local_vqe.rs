//! Explicit local assets only; this evaluator never downloads a model.
use super::*;
use libloading::Library;
use sha2::{Digest, Sha256};
use std::{
    ffi::{CString, OsString, c_char, c_int},
    io::Read,
    path::Path,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const V12_SHA: &str = "4856ecf5f522b23fb2bc5caeac81f323c0ef1c4c156a9c7d40a6adbe092ba9ce";
const V14_SHA: &str = "b6e43138588a83bfe903ab5e143b4020b91c1e1629f5a575ac5855ff0003c731";

struct Model {
    name: &'static str,
    sha256: String,
    joint_noise_reduction: bool,
}
fn verify_model(path: &Path) -> Result<Model> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    if !matches!(size, 5_173_088 | 2_924_224) {
        return Err("unapproved model size".into());
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(5_173_089).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != size {
        return Err("model size changed during verification".into());
    }
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let (name, joint_noise_reduction) = match (size, sha256.as_str()) {
        (5_173_088, V12_SHA) => ("localvqe-v1.2-1.3M-f32", true),
        (2_924_224, V14_SHA) => ("localvqe-v1.4-aec-200K-f32", false),
        _ => return Err("unapproved model checksum".into()),
    };
    Ok(Model {
        name,
        sha256,
        joint_noise_reduction,
    })
}

type NewOptions = unsafe extern "C" fn() -> usize;
type FreeOptions = unsafe extern "C" fn(usize);
type SetString = unsafe extern "C" fn(usize, *const c_char) -> c_int;
type SetThreads = unsafe extern "C" fn(usize, c_int) -> c_int;
type NewContext = unsafe extern "C" fn(usize) -> usize;
type FreeContext = unsafe extern "C" fn(usize);
type GetInt = unsafe extern "C" fn(usize) -> c_int;
type Process = unsafe extern "C" fn(usize, *const f32, *const f32, c_int, *mut f32) -> c_int;
type NoiseGate = unsafe extern "C" fn(usize, c_int, f32) -> c_int;
struct Engine {
    context: usize,
    process: Process,
    free: FreeContext,
    _library: Library,
}
impl Engine {
    fn new(library_path: &Path, model_path: &Path) -> Result<Self> {
        let path = CString::new(model_path.to_str().ok_or("model path must be UTF-8")?)?;
        // SAFETY: the caller explicitly supplies a trusted built library. The
        // ABI below is pinned to LocalVQE7f61cd0's localvqe_api.h, whose opaque
        // handles are uintptr_t. Symbols are copied while Library stays owned.
        let library = unsafe { Library::new(library_path)? };
        macro_rules! symbol {
            ($name:literal,$ty:ty) => {
                unsafe { *library.get::<$ty>(concat!($name, "\0").as_bytes())? }
            };
        }
        let new_options = symbol!("localvqe_options_new", NewOptions);
        let free_options = symbol!("localvqe_options_free", FreeOptions);
        let set_path = symbol!("localvqe_options_set_model_path", SetString);
        let set_backend = symbol!("localvqe_options_set_backend", SetString);
        let set_threads = symbol!("localvqe_options_set_threads", SetThreads);
        let new_context = symbol!("localvqe_new_with_options", NewContext);
        let free = symbol!("localvqe_free", FreeContext);
        let process = symbol!("localvqe_process_f32", Process);
        let sample_rate = symbol!("localvqe_sample_rate", GetInt);
        let hop_length = symbol!("localvqe_hop_length", GetInt);
        let noise_gate = symbol!("localvqe_set_noise_gate", NoiseGate);
        // SAFETY: options/context handles remain live through their respective
        // calls; CStrings remain allocated, and options setters copy the path.
        let context = unsafe {
            let options = new_options();
            if options == 0 {
                return Err("LocalVQE options allocation failed".into());
            }
            let configured = set_path(options, path.as_ptr()) == 0
                && set_backend(options, c"CPU".as_ptr()) == 0
                && set_threads(options, 2) == 0;
            let context = if configured { new_context(options) } else { 0 };
            free_options(options);
            context
        };
        if context == 0 {
            return Err("LocalVQE model initialization failed".into());
        }
        let engine = Self {
            context,
            process,
            free,
            _library: library,
        };
        // SAFETY: engine owns a live context. Any refusal drops it before return.
        if unsafe {
            sample_rate(context) != RATE as c_int
                || hop_length(context) != 256
                || noise_gate(context, 0, -90.0) != 0
        } {
            return Err("LocalVQE runtime shape or noise-gate configuration refused".into());
        }
        Ok(engine)
    }
    fn process(&mut self, mic: &[f32], reference: &[f32]) -> Result<Vec<f32>> {
        validate_pcm(mic, reference)?;
        let count = c_int::try_from(mic.len())?;
        let mut out = vec![f32::NAN; mic.len()];
        // SAFETY: all three distinct buffers have count samples and out is
        // writable. Context and library remain owned throughout this call.
        let code = unsafe {
            (self.process)(
                self.context,
                mic.as_ptr(),
                reference.as_ptr(),
                count,
                out.as_mut_ptr(),
            )
        };
        if code != 0 || out.iter().any(|x| !x.is_finite()) {
            return Err("LocalVQE refused input or returned incomplete/nonfinite output".into());
        }
        Ok(out)
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: exactly one owned context; library drops after this destructor.
        unsafe { (self.free)(self.context) };
    }
}
fn validate_pcm(mic: &[f32], reference: &[f32]) -> Result<()> {
    if mic.len() < 512
        || mic.len() != reference.len()
        || mic
            .iter()
            .chain(reference)
            .any(|x| !x.is_finite() || x.abs() > 1.0)
    {
        return Err("invalid LocalVQE PCM shape/range".into());
    }
    Ok(())
}

pub(super) fn run(args: &[OsString]) -> Result<()> {
    if args.len() != 2 {
        return Err("use --local-vqe-signals TRUSTED_LIBRARY VERIFIED_MODEL".into());
    }
    let library = Path::new(&args[0]);
    let model_path = Path::new(&args[1]);
    // Validate the allowlisted model before loading any native code.
    let model = verify_model(model_path)?;
    let mut fixtures = Vec::new();
    for (label, far_seed, near_seed, taps) in [
        (
            "original",
            127,
            915,
            [(640, 0.6), (1120, 0.25), (1560, 0.1)],
        ),
        (
            "unseen-short",
            621,
            8901,
            [(96, 0.5), (512, 0.24), (1408, 0.08)],
        ),
        (
            "unseen-long",
            7311,
            8932,
            [(224, 0.45), (992, 0.25), (2016, 0.14)],
        ),
    ] {
        let far = signal(far_seed);
        let near = signal(near_seed);
        let zero = vec![0.0; SAMPLES];
        let echo = room_response(&far, &taps, 0.0);
        let mut results = Vec::new();
        for backend in ["passthrough", model.name] {
            let mut lag = 0;
            for (case, clean, reference) in [
                ("near-only", &near, &zero),
                ("echo-only", &zero, &far),
                ("double-talk", &near, &far),
            ] {
                let mic: Vec<_> = clean
                    .iter()
                    .zip(if case == "near-only" { &zero } else { &echo })
                    .map(|(n, e)| n + e)
                    .collect();
                let started = Instant::now();
                let out = if backend == "passthrough" {
                    mic.clone()
                } else {
                    Engine::new(library, model_path)?.process(&mic, reference)?
                };
                if case == "near-only" {
                    lag = delay(&near, &out);
                }
                results.push(serde_json::json!({"backend":backend,"case":case,"metrics":metrics(clean,if case=="near-only" {&zero}else{&echo},&out,lag),"wall_ms_including_model_load":started.elapsed().as_secs_f64()*1000.0}));
            }
        }
        fixtures.push(serde_json::json!({"fixture":label,"far_seed":far_seed,"near_seed":near_seed,"room_taps":taps,"signal_qualification":quality_report(&results),"results":results}));
    }
    println!(
        "{}",
        serde_json::json!({"hardware":hardware(),"synthetic_only":true,"synthetic_near_is_not_speech":true,"model":model.name,"model_sha256":model.sha256,"joint_noise_reduction":model.joint_noise_reduction,"additional_noise_gate_enabled":false,"cpu_threads":2,"sample_rate_hz":RATE,"audio_seconds_per_case":SAMPLES/RATE,"warmup_seconds":WARMUP/RATE,"metric_tail_excluded_samples":FRAME*2,"fixtures":fixtures,"aggregate":aggregate_quality(&fixtures),"candidate_selected_for_app":false,"clock_fault_recovery_qualified":false,"speech_retention_qualified":false,"backend_tail_drained":false})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an explicitly supplied trusted local runtime; no models"]
    fn supplied_native_runtime_rejects_a_missing_model() {
        let library =
            std::env::var_os("FOTW_LOCALVQE_LIBRARY").expect("provide the owned runtime library");
        let missing =
            std::env::temp_dir().join(format!("fotw-localvqe-absent-{}.gguf", std::process::id()));
        assert!(!missing.exists());
        let error = Engine::new(Path::new(&library), &missing)
            .err()
            .expect("missing model must fail")
            .to_string();
        assert!(error.contains("model initialization failed"), "{error}");
    }
    #[test]
    fn invalid_model_is_rejected_before_loading_native_code() {
        let path = std::env::temp_dir().join(format!("fotw-localvqe-empty-{}", std::process::id()));
        std::fs::write(&path, []).unwrap();
        let result = run(&[
            OsString::from("/no/such/native-library"),
            path.clone().into_os_string(),
        ]);
        std::fs::remove_file(path).unwrap();
        let error = result.unwrap_err().to_string();
        assert!(error.contains("unapproved model size"), "{error}");
    }
    #[test]
    fn correct_length_does_not_admit_unknown_weights() {
        let path =
            std::env::temp_dir().join(format!("fotw-localvqe-wrong-digest-{}", std::process::id()));
        std::fs::write(&path, vec![0; 5_173_088]).unwrap();
        let result = run(&[
            OsString::from("/no/such/native-library"),
            path.clone().into_os_string(),
        ]);
        std::fs::remove_file(path).unwrap();
        let error = result.unwrap_err().to_string();
        assert!(error.contains("unapproved model checksum"), "{error}");
    }
    #[test]
    fn malformed_pcm_is_refused_before_the_native_call() {
        let valid = vec![0.0; 512];
        assert!(validate_pcm(&valid, &valid).is_ok());
        assert!(validate_pcm(&valid[..511], &valid[..511]).is_err());
        assert!(validate_pcm(&valid, &valid[..511]).is_err());
        for sample in [f32::NAN, f32::INFINITY, 1.01, -1.01] {
            let mut invalid = valid.clone();
            invalid[300] = sample;
            assert!(validate_pcm(&invalid, &valid).is_err());
            assert!(validate_pcm(&valid, &invalid).is_err());
        }
    }
}
