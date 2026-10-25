//! Volatile native plan state; downstream agents remain the history authority.

use std::collections::BTreeMap;

use agent_client_protocol::schema::v2;
use serde_json::{Value, json};

#[derive(Default)]
pub(crate) struct Plans {
    current: BTreeMap<String, v2::PlanUpdate>,
}

impl Plans {
    pub fn ingest(&mut self, update: &v2::PlanUpdate) {
        // Inspect the serialized discriminator contract so future content variants survive too.
        if let Ok(value) = serde_json::to_value(&update.plan)
            && let Some(id) = value["planId"].as_str()
        {
            self.current.insert(id.to_owned(), update.clone());
        }
    }

    pub fn snapshot(&self, seq: u64) -> Value {
        json!({"version": 1, "seq": seq, "support": "supported",
            "observation": if self.current.is_empty() { "not_observed" } else { "observed" },
            "plans": self.current})
    }

    pub fn updates(&self) -> impl Iterator<Item = v2::SessionUpdate> + '_ {
        self.current
            .values()
            .cloned()
            .map(v2::SessionUpdate::PlanUpdate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(plan: Value) -> v2::PlanUpdate {
        serde_json::from_value(json!({"plan":plan,"_meta":{"envelope":"retained"}})).unwrap()
    }

    #[test]
    fn replacements_preserve_order_metadata_and_unknown_values_without_merging_entries() {
        let mut plans = Plans::default();
        assert_eq!(plans.snapshot(0)["support"], "supported");
        assert_eq!(plans.snapshot(0)["observation"], "not_observed");
        let entry = |content, status| json!({"content":content,"priority":"future_priority","status":status,"_meta":{"id":content}});
        let first = update(
            json!({"type":"items","planId":"steps","entries":[entry("A","pending"),entry("B","in_progress")],"_meta":{"provider":"retained"}}),
        );
        plans.ingest(&first);
        assert_eq!(
            plans.snapshot(1)["plans"]["steps"],
            serde_json::to_value(&first).unwrap()
        );
        let second = update(
            json!({"type":"items","planId":"steps","entries":[entry("B","cancelled"),entry("A","future_status")]}),
        );
        plans.ingest(&second);
        assert_eq!(
            plans.snapshot(2)["plans"]["steps"],
            serde_json::to_value(&second).unwrap()
        );
        let custom = update(
            json!({"type":"_custom","planId":"other","nested":{"order":[2,1]},"_meta":{"opaque":true}}),
        );
        plans.ingest(&custom);
        let empty = update(json!({"type":"items","planId":"steps","entries":[]}));
        plans.ingest(&empty);
        let snapshot = plans.snapshot(9);
        assert_eq!(snapshot["seq"], 9);
        assert_eq!(snapshot["observation"], "observed");
        assert_eq!(
            snapshot["plans"]["other"],
            serde_json::to_value(custom).unwrap()
        );
        assert_eq!(
            snapshot["plans"]["steps"],
            serde_json::to_value(empty).unwrap()
        );
        assert_eq!(plans.updates().count(), 2);
    }

    #[test]
    fn malformed_plans_follow_the_pinned_sdk_boundary_without_inventing_valid_content() {
        for plan in [
            json!({"type":"items","entries":[]}),
            json!({"type":"items","planId":42,"entries":[]}),
            json!({"type":false,"planId":"bad"}),
            json!({"type":"items","planId":"bad"}),
        ] {
            assert!(serde_json::from_value::<v2::PlanUpdate>(json!({"plan":plan})).is_err());
        }
        // The SDK deliberately skips invalid entries rather than rejecting a whole notification.
        let normalized = update(
            json!({"type":"items","planId":"tolerant","entries":[{"invalid":true},{"content":"valid","priority":"low","status":"pending"}]}),
        );
        let mut plans = Plans::default();
        plans.ingest(&normalized);
        assert_eq!(
            plans.snapshot(1)["plans"]["tolerant"]["plan"]["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
