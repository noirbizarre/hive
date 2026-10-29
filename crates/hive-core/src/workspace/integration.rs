//! Global optional integration policy; executable probing and marker formats live outside core.

use serde::{Deserialize, Serialize};

use super::WorkspaceError;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktrunkConfig {
    pub enabled: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorktrunkAvailability {
    Usable,
    Unavailable,
    Incompatible,
}

/// WorkTrunk selection requires both lifecycle delegation and Hive activity publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrationSelection {
    Native,
    Worktrunk,
}

impl WorktrunkConfig {
    /// Probe on the owning node only when allowed; disabled mode must not invoke `wt`.
    pub fn select(
        self,
        probe: impl FnOnce() -> WorktrunkAvailability,
    ) -> Result<IntegrationSelection, WorkspaceError> {
        if self.enabled == Some(false) {
            return Ok(IntegrationSelection::Native);
        }
        let availability = probe();
        if availability == WorktrunkAvailability::Usable {
            Ok(IntegrationSelection::Worktrunk)
        } else if self.enabled == Some(true) {
            Err(WorkspaceError::WorktrunkRequired { availability })
        } else {
            Ok(IntegrationSelection::Native)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, Deserialize)]
    struct GlobalContract {
        #[serde(default)]
        worktrunk: WorktrunkConfig,
    }

    #[test]
    fn omitted_and_empty_sections_auto_detect_without_silently_defaulting_to_disabled() {
        for text in ["", "[worktrunk]"] {
            let config: GlobalContract = toml::from_str(text).unwrap();
            assert_eq!(config.worktrunk.enabled, None);
            for availability in [
                WorktrunkAvailability::Usable,
                WorktrunkAvailability::Unavailable,
                WorktrunkAvailability::Incompatible,
            ] {
                let expected = if availability == WorktrunkAvailability::Usable {
                    IntegrationSelection::Worktrunk
                } else {
                    IntegrationSelection::Native
                };
                assert_eq!(config.worktrunk.select(|| availability).unwrap(), expected);
            }
        }
    }

    #[test]
    fn explicit_disablement_never_probes_and_explicit_enablement_never_falls_back() {
        let disabled: GlobalContract = toml::from_str("[worktrunk]\nenabled = false").unwrap();
        assert_eq!(
            disabled
                .worktrunk
                .select(|| panic!("disabled mode probed wt"))
                .unwrap(),
            IntegrationSelection::Native
        );
        let required: GlobalContract = toml::from_str("[worktrunk]\nenabled = true").unwrap();
        assert_eq!(
            required
                .worktrunk
                .select(|| WorktrunkAvailability::Usable)
                .unwrap(),
            IntegrationSelection::Worktrunk
        );
        for availability in [
            WorktrunkAvailability::Unavailable,
            WorktrunkAvailability::Incompatible,
        ] {
            let error = required.worktrunk.select(|| availability).unwrap_err();
            assert_eq!(error.code(), "hive::workspace::worktrunk_required");
            assert!(error.to_string().contains("install a compatible wt"));
        }
        assert!(toml::from_str::<GlobalContract>("[worktrunk]\nenabled = 'auto'").is_err());
        assert!(toml::from_str::<GlobalContract>("[worktrunk]\nprofile = 'agent'").is_err());
    }
}
