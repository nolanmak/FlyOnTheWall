//! Bundled artifact inventory for explicit local-summary provisioning.
//!
//! Artifact availability is not task-quality qualification. These records do
//! not create recommendation evidence or a Verified-on-this-Mac badge.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Pinned bytes in a model or runtime package.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogFile {
    /// Safe local basename, independent of the upstream folder structure.
    pub name: String,
    /// Relative path in the pinned source repository/archive.
    pub source_path: String,
    /// Exact expected length, never a runtime RAM estimate.
    pub bytes: u64,
    /// Lowercase SHA-256 of the regular file contents.
    pub sha256: String,
}

/// One optional public model package. No parameter/download-size ceiling.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModel {
    /// Stable selection key.
    pub id: String,
    /// User-visible name.
    pub label: String,
    /// Quantization of these exact artifacts.
    pub quantization: String,
    /// Upstream Hugging Face owner/repository.
    pub repository: String,
    /// Immutable upstream git revision; never `main` or a registry tag.
    pub revision: String,
    /// License identifier shown before downloading.
    pub license: String,
    /// First GGUF followed by all remaining shards.
    pub files: Vec<CatalogFile>,
}
impl CatalogModel {
    /// Exact total package download size. Validated on catalog load.
    pub fn download_bytes(&self) -> Option<u64> {
        self.files
            .iter()
            .try_fold(0_u64, |n, f| n.checked_add(f.bytes))
    }
    /// Identity of the complete file set, stable under manifest reordering.
    /// Friendly labels/source metadata do not substitute for content identity.
    pub fn package_sha256(&self) -> String {
        let mut entries: Vec<_> = self
            .files
            .iter()
            .map(|f| (&f.name, f.bytes, &f.sha256))
            .collect();
        entries.sort();
        let encoded = serde_json::to_vec(&entries).expect("file identities serialize");
        Sha256::digest(encoded)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    /// Public immutable source URL. Validated source components contain no
    /// credentials/query/fragment or path traversal.
    fn file_url(&self, file: &CatalogFile) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repository, self.revision, file.source_path
        )
    }
}

/// Pinned optional llama.cpp release, not an installed user daemon.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRuntime {
    /// Exact supported adapter protocol.
    pub protocol: String,
    /// Immutable source commit.
    pub source_revision: String,
    /// Upstream release tag; archive content is independently pinned below.
    pub tag: String,
    /// Published macOS ARM64 archive basename.
    pub archive_name: String,
    /// Exact compressed bytes.
    pub archive_bytes: u64,
    /// Expected archive SHA-256.
    pub archive_sha256: String,
    /// Bounded extra disk required by extraction and regular-file staging.
    pub unpack_extra_bytes: u64,
    /// License identifier shown before downloading.
    pub license: String,
    /// All regular runtime files after resolving pinned archive aliases.
    pub files: Vec<CatalogFile>,
    /// License files retained alongside the installed runtime.
    pub notices: Vec<CatalogFile>,
}
impl CatalogRuntime {
    /// Public pinned release URL; redirects remain the downloader's separate
    /// allowlist responsibility.
    pub fn archive_url(&self) -> String {
        format!(
            "https://github.com/ggml-org/llama.cpp/releases/download/{}/{}",
            self.tag, self.archive_name
        )
    }
}

/// Download authority created only by the validated bundled catalog. Client
/// requests select IDs, not arbitrary URLs, hashes or destination filenames.
#[derive(Clone, Debug)]
pub struct DownloadSpec {
    name: String,
    url: String,
    bytes: u64,
    sha256: String,
}
impl DownloadSpec {
    /// Safe filename within the installer's owned staging directory.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Exact public HTTPS source; no credentials or mutable model tag.
    pub fn url(&self) -> &str {
        &self.url
    }
    /// Exact expected bytes.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    /// Content identity checked before promotion.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format_version: u32,
    revision: String,
    runtime: CatalogRuntime,
    models: Vec<CatalogModel>,
}

/// Validated inventory. Only the app-bundled manifest constructs this type;
/// parsing arbitrary external JSON is not an authentication mechanism.
pub struct Catalog {
    data: Manifest,
}
impl Catalog {
    /// Read the offline bundled catalog; performs no networking or disk access.
    ///
    /// # Errors
    /// A malformed/inconsistent bundled manifest fails closed.
    pub fn bundled() -> Result<Self, String> {
        parse(include_str!("catalog.json"))
    }
    /// Catalog revision for install receipts, not a permanently stored RAM tier.
    pub fn revision(&self) -> &str {
        &self.data.revision
    }
    /// Exact runtime package.
    pub fn runtime(&self) -> &CatalogRuntime {
        &self.data.runtime
    }
    /// Available artifact descriptions, which are not recommendations.
    pub fn models(&self) -> &[CatalogModel] {
        &self.data.models
    }
    /// Resolve a stable model key; never use a path supplied by a client.
    pub fn model(&self, id: &str) -> Option<&CatalogModel> {
        self.data.models.iter().find(|m| m.id == id)
    }
    /// Resolve download authority from a model ID in this catalog snapshot.
    pub fn model_downloads(&self, id: &str) -> Option<Vec<DownloadSpec>> {
        let model = self.model(id)?;
        Some(
            model
                .files
                .iter()
                .map(|file| DownloadSpec {
                    name: file.name.clone(),
                    url: model.file_url(file),
                    bytes: file.bytes,
                    sha256: file.sha256.clone(),
                })
                .collect(),
        )
    }
    /// Resolve the runtime archive; extraction checks its pinned file set.
    pub fn runtime_download(&self) -> DownloadSpec {
        let runtime = self.runtime();
        DownloadSpec {
            name: runtime.archive_name.clone(),
            url: runtime.archive_url(),
            bytes: runtime.archive_bytes,
            sha256: runtime.archive_sha256.clone(),
        }
    }
}
fn parse(bytes: &str) -> Result<Catalog, String> {
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("Local model catalog exceeds metadata limit".into());
    }
    let data: Manifest = serde_json::from_str(bytes).map_err(|_| "Invalid local model catalog")?;
    if data.format_version != 1 || !identifier(&data.revision) {
        return Err("Unsupported local catalog version".into());
    }
    let runtime = &data.runtime;
    if runtime.protocol != "llama_cpp_http_v1"
        || !hex(&runtime.source_revision, 40)
        || !identifier(&runtime.tag)
        || runtime.archive_name != format!("llama-{}-bin-macos-arm64.tar.gz", runtime.tag)
        || runtime.archive_bytes == 0
        || !hex(&runtime.archive_sha256, 64)
        || !text(&runtime.license, 256)
        || runtime.files.is_empty()
        || runtime.notices.is_empty()
    {
        return Err("Invalid pinned local runtime".into());
    }
    validate_files(runtime.files.iter().chain(&runtime.notices), false)?;
    if !runtime.files.iter().any(|f| f.name == "llama-server")
        || runtime
            .files
            .iter()
            .any(|f| f.name != "llama-server" && !f.name.ends_with(".dylib"))
    {
        return Err("Incomplete local runtime file set".into());
    }
    // Extraction holds each distinct source plus the flattened regular output.
    // Count both; aliases may share a source but cannot change its identity.
    let mut sources = std::collections::BTreeMap::new();
    let mut flat = 0_u64;
    for f in runtime.files.iter().chain(&runtime.notices) {
        flat = flat
            .checked_add(f.bytes)
            .ok_or("Runtime staging size overflow")?;
        if let Some(previous) = sources.insert(&f.source_path, (f.bytes, &f.sha256))
            && previous != (f.bytes, &f.sha256)
        {
            return Err("Runtime aliases disagree on source identity".into());
        }
    }
    let source_bytes = sources
        .values()
        .try_fold(0_u64, |n, (size, _)| n.checked_add(*size))
        .ok_or("Runtime staging size overflow")?;
    let extra = flat
        .checked_add(source_bytes)
        .ok_or("Runtime staging size overflow")?;
    if runtime.unpack_extra_bytes < extra {
        return Err("Runtime unpack allowance omits staging bytes".into());
    }
    let mut ids = std::collections::HashSet::new();
    for model in &data.models {
        if !identifier(&model.id)
            || !ids.insert(model.id.to_ascii_lowercase())
            || !text(&model.label, 256)
            || !identifier(&model.quantization)
            || !hex(&model.revision, 40)
            || !text(&model.license, 256)
            || model.repository.split('/').count() != 2
            || !model.repository.split('/').all(filename)
            || model.files.is_empty()
            || model.download_bytes().is_none()
        {
            return Err("Invalid pinned local model".into());
        }
        validate_files(model.files.iter(), true)?;
    }
    Ok(Catalog { data })
}
fn text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn component(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn identifier(value: &str) -> bool {
    component(value, 128)
}
fn filename(value: &str) -> bool {
    component(value, 255)
}
fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate_files<'a>(
    files: impl Iterator<Item = &'a CatalogFile>,
    model: bool,
) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    for f in files {
        if !filename(&f.name)
            || !names.insert(f.name.to_ascii_lowercase())
            || f.bytes == 0
            || !hex(&f.sha256, 64)
            || f.source_path.len() > 1024
            || !f.source_path.split('/').all(filename)
            || (model
                && (!f.name.ends_with(".gguf")
                    || f.source_path.rsplit('/').next() != Some(f.name.as_str())))
            || (!model && f.source_path.contains('/'))
        {
            return Err("Invalid catalog file identity or path".into());
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn test_catalog(json: &str) -> Result<Catalog, String> {
    parse(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn fixture() -> Value {
        json!({"format_version":1,"revision":"test-1",
        "runtime":{"protocol":"llama_cpp_http_v1","source_revision":"a".repeat(40),"tag":"b11946",
            "archive_name":"llama-b11946-bin-macos-arm64.tar.gz","archive_bytes":100,"archive_sha256":"b".repeat(64),
            "unpack_extra_bytes":500,"license":"MIT","files":[{"name":"llama-server","source_path":"llama-server","bytes":100,"sha256":"c".repeat(64)}],
            "notices":[{"name":"LICENSE","source_path":"LICENSE","bytes":10,"sha256":"d".repeat(64)}]},
        "models":[{"id":"large-model","label":"Large model","quantization":"Q4_K_M","repository":"Owner/Model-GGUF",
            "revision":"e".repeat(40),"license":"Apache-2.0","files":[
                {"name":"model-00001-of-00002.gguf","source_path":"Q4_K_M/model-00001-of-00002.gguf","bytes":100_u64<<30,"sha256":"1".repeat(64)},
                {"name":"model-00002-of-00002.gguf","source_path":"Q4_K_M/model-00002-of-00002.gguf","bytes":60_u64<<30,"sha256":"2".repeat(64)}]}]})
    }
    fn load(value: &Value) -> Result<Catalog, String> {
        parse(&value.to_string())
    }
    #[test]
    fn accepts_large_packages_without_a_laptop_or_download_size_ceiling() {
        let c = load(&fixture()).unwrap();
        let model = c.model("large-model").unwrap();
        assert_eq!(model.download_bytes(), Some(160_u64 << 30));
        assert_eq!(
            model.file_url(&model.files[0]),
            format!(
                "https://huggingface.co/Owner/Model-GGUF/resolve/{}/Q4_K_M/model-00001-of-00002.gguf",
                "e".repeat(40)
            )
        );
    }
    #[test]
    fn rejects_mutable_revisions_missing_licenses_and_unknown_protocols() {
        for path in [
            "/models/0/revision",
            "/models/0/license",
            "/runtime/license",
            "/runtime/source_revision",
            "/runtime/protocol",
        ] {
            let mut f = fixture();
            *f.pointer_mut(path).unwrap() = json!(if path.ends_with("license") {
                ""
            } else {
                "main"
            });
            assert!(load(&f).is_err(), "{path}");
        }
    }
    #[test]
    fn rejects_traversal_urls_and_ambiguous_local_filenames() {
        for value in [
            "../model.gguf",
            "/model.gguf",
            "Q4_K_M/../model.gguf",
            "Q4_K_M/%2e%2e/model.gguf",
            "model.gguf?token=x",
        ] {
            let mut f = fixture();
            f["models"][0]["files"][0]["source_path"] = json!(value);
            assert!(load(&f).is_err(), "{value}");
        }
        let mut f = fixture();
        f["models"][0]["files"][1]["name"] = f["models"][0]["files"][0]["name"].clone();
        assert!(load(&f).is_err());
    }
    #[test]
    fn rejects_bad_digests_lengths_duplicates_and_overflow() {
        for path in [
            "/models/0/files/0/sha256",
            "/runtime/archive_sha256",
            "/runtime/files/0/sha256",
        ] {
            let mut f = fixture();
            *f.pointer_mut(path).unwrap() = json!("unknown");
            assert!(load(&f).is_err(), "{path}");
        }
        let mut f = fixture();
        f["models"][0]["files"][0]["bytes"] = json!(0);
        assert!(load(&f).is_err());
        f = fixture();
        f["models"][0]["files"][0]["bytes"] = json!(u64::MAX);
        assert!(load(&f).is_err());
        f = fixture();
        let duplicate = f["models"][0].clone();
        f["models"].as_array_mut().unwrap().push(duplicate);
        assert!(load(&f).is_err());
    }
    #[test]
    fn full_package_identity_covers_every_shard_and_is_order_independent() {
        let a = load(&fixture()).unwrap();
        let mut f = fixture();
        f["models"][0]["files"].as_array_mut().unwrap().reverse();
        assert_eq!(
            a.models()[0].package_sha256(),
            load(&f).unwrap().models()[0].package_sha256()
        );
        f["models"][0]["files"][0]["sha256"] = json!("3".repeat(64));
        assert_ne!(
            a.models()[0].package_sha256(),
            load(&f).unwrap().models()[0].package_sha256()
        );
    }
    #[test]
    fn incomplete_runtime_or_empty_model_cannot_be_installed() {
        let mut f = fixture();
        f["runtime"]["files"] = json!([]);
        assert!(load(&f).is_err());
        f = fixture();
        f["models"][0]["files"] = json!([]);
        assert!(load(&f).is_err());
        f = fixture();
        f["format_version"] = json!(99);
        assert!(load(&f).is_err());
    }
    #[test]
    fn bundled_inventory_contains_pinned_small_and_large_artifacts_not_recommendations() {
        let catalog = Catalog::bundled().unwrap();
        assert_eq!(catalog.models().len(), 4);
        assert!(catalog.model_downloads("../untrusted").is_none());
        let small = catalog.model_downloads("qwen3-06b-q8-0").unwrap();
        assert_eq!(small.len(), 1);
        assert_eq!(small[0].bytes(), 639_446_688);
        let large = catalog.model_downloads("qwen3-235b-a22b-q4-k-m").unwrap();
        assert_eq!(large.len(), 5);
        assert!(large.iter().map(DownloadSpec::bytes).sum::<u64>() > 128_u64 << 30);
        assert_eq!(catalog.runtime_download().bytes(), 16_438_371);
        assert_eq!(
            catalog.runtime_download().sha256(),
            "d618da5e7bbb19077f086e6a7e1d77eab55b9bfd481f7531aadada86d13db9ed"
        );
        let mut forged = fixture();
        forged["models"][0]["recommended"] = json!(true);
        assert!(
            load(&forged).is_err(),
            "metadata cannot add a qualification claim"
        );
    }
}
