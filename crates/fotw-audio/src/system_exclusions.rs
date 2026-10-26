//! Pure planning for native process exclusions. No device or process queries here.
use crate::{AppRef, TapError};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ExclusionPlan {
    pub(crate) process_objects: Vec<u32>,
    pub(crate) bundle_ids: Vec<String>,
}
/// Translate process identities before constructing a native tap descriptor.
pub(crate) fn resolve(
    requested: &[AppRef],
    bundle_supported: bool,
    mut object_for_pid: impl FnMut(u32) -> Result<u32, TapError>,
) -> Result<ExclusionPlan, TapError> {
    use std::collections::BTreeSet;
    let mut pids = BTreeSet::new();
    let mut bundles = BTreeSet::new();
    // Validate the entire request before any process lookup.
    for app in requested {
        match app {
            AppRef::Pid(pid) => {
                pids.insert(*pid);
            }
            AppRef::BundleId(id) => {
                if !bundle_supported {
                    return Err(TapError::unsupported(
                        "persistent application exclusions require macOS 26 or newer",
                    ));
                }
                if id.is_empty()
                    || id.len() > 255
                    || !id.contains('.')
                    || !id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
                {
                    return Err(TapError::unsupported(
                        "invalid application bundle identifier",
                    ));
                }
                bundles.insert(id.clone());
            }
            AppRef::Name(_) => {
                return Err(TapError::unsupported(
                    "application names are not stable capture identities; use a bundle identifier",
                ));
            }
        }
    }
    let mut objects = BTreeSet::new();
    for pid in pids {
        let object = object_for_pid(pid)?;
        if object == 0 {
            return Err(TapError::platform("process has no Core Audio object"));
        }
        objects.insert(object);
    }
    Ok(ExclusionPlan {
        process_objects: objects.into_iter().collect(),
        bundle_ids: bundles.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_process_objects_are_resolved_instead_of_passing_unix_pids() {
        let mut seen = Vec::new();
        let plan = resolve(&[AppRef::Pid(9001)], true, |pid| {
            seen.push(pid);
            Ok(42)
        })
        .unwrap();
        assert_eq!(plan.process_objects, [42]);
        assert_eq!(seen, [9001]);
    }
    #[test]
    fn stable_bundle_selectors_are_preserved_without_pid_resolution() {
        let plan = resolve(
            &[
                AppRef::BundleId("com.brave.Browser".into()),
                AppRef::BundleId("com.spotify.client".into()),
            ],
            true,
            |_| panic!("bundle scope must not resolve an incidental PID"),
        )
        .unwrap();
        assert_eq!(plan.bundle_ids, ["com.brave.Browser", "com.spotify.client"]);
        assert!(plan.process_objects.is_empty());
    }
    #[test]
    fn unsupported_bundle_selection_refuses_instead_of_capturing_an_unfiltered_mix() {
        assert!(
            resolve(
                &[AppRef::BundleId("com.spotify.client".into())],
                false,
                |_| panic!("no native resolution on unsupported policy")
            )
            .is_err()
        );
    }
    #[test]
    fn unsupported_names_invalid_bundle_ids_and_unknown_processes_are_not_silently_dropped() {
        for app in [
            AppRef::Name("Brave".into()),
            AppRef::BundleId(String::new()),
            AppRef::BundleId("com.brave.*".into()),
            AppRef::BundleId(" com.spotify.client".into()),
        ] {
            assert!(resolve(&[app], true, |_| Ok(1)).is_err());
        }
        assert!(
            resolve(&[AppRef::Pid(9)], true, |_| Err(TapError::platform(
                "process vanished"
            )))
            .is_err()
        );
        assert!(resolve(&[AppRef::Pid(9)], true, |_| Ok(0)).is_err());
    }
    #[test]
    fn duplicate_selectors_are_canonical_and_do_not_repeat_native_resolution() {
        let mut calls = 0;
        let plan = resolve(
            &[
                AppRef::Pid(7),
                AppRef::Pid(7),
                AppRef::BundleId("com.spotify.client".into()),
                AppRef::BundleId("com.spotify.client".into()),
            ],
            true,
            |_| {
                calls += 1;
                Ok(42)
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(plan.process_objects, [42]);
        assert_eq!(plan.bundle_ids, ["com.spotify.client"]);
    }
    #[test]
    fn an_empty_exclusion_plan_keeps_the_default_mix_without_native_resolution() {
        let plan = resolve(&[], false, |_| panic!("no process requested")).unwrap();
        assert!(plan.process_objects.is_empty() && plan.bundle_ids.is_empty());
    }
}
