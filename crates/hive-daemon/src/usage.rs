//! Turn-level ACP accounting. ACP does not identify individual billable model calls.

use agent_client_protocol::schema::{v1, v2};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Tokens {
    pub total_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub thought_tokens: Option<u64>,
    pub cached_read_tokens: Option<u64>,
    pub cached_write_tokens: Option<u64>,
}

impl From<&v1::Usage> for Tokens {
    fn from(usage: &v1::Usage) -> Self {
        Self {
            total_tokens: Some(usage.total_tokens),
            input_tokens: Some(usage.input_tokens),
            output_tokens: Some(usage.output_tokens),
            thought_tokens: usage.thought_tokens,
            cached_read_tokens: usage.cached_read_tokens,
            cached_write_tokens: usage.cached_write_tokens,
        }
    }
}

impl From<&v2::Usage> for Tokens {
    fn from(usage: &v2::Usage) -> Self {
        Self {
            total_tokens: Some(usage.total_tokens),
            input_tokens: Some(usage.input_tokens),
            output_tokens: Some(usage.output_tokens),
            thought_tokens: usage.thought_tokens,
            cached_read_tokens: usage.cached_read_tokens,
            cached_write_tokens: usage.cached_write_tokens,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Money {
    pub amount: f64,
    pub currency: String,
}

impl Money {
    pub fn valid(amount: f64, currency: &str) -> Option<Self> {
        (amount.is_finite()
            && amount >= 0.0
            && currency.len() == 3
            && currency.bytes().all(|byte| byte.is_ascii_uppercase()))
        .then(|| Self {
            amount,
            currency: currency.to_owned(),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Estimate {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub total: Option<f64>,
    pub currency: String,
    pub provider: String,
    pub model: String,
    pub retrieved_at: u64,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Turn {
    pub id: String,
    pub agent: String,
    pub timestamp: u64,
    pub tokens: Option<Tokens>,
    pub estimate: Option<Estimate>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Ledger {
    pub turns: Vec<Turn>,
    pub actual_session_cost: Option<Money>,
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary {
    pub turns: usize,
    pub reported_turns: usize,
    pub tokens: Tokens,
    pub estimated_cost: Option<Money>,
    pub actual_session_cost: Option<Money>,
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
}

impl Summary {
    pub fn combine(&mut self, other: &Self) {
        if self.turns == 0 && self.actual_session_cost.is_none() {
            *self = other.clone();
            return;
        }
        if other.turns == 0 && other.actual_session_cost.is_none() {
            return;
        }
        let add = |left: Option<u64>, right: Option<u64>| left?.checked_add(right?);
        let money = |left: &Option<Money>, right: &Option<Money>| {
            let (left, right) = (left.as_ref()?, right.as_ref()?);
            (left.currency == right.currency)
                .then(|| Money::valid(left.amount + right.amount, &left.currency))
                .flatten()
        };
        let had_turns = self.turns > 0;
        self.turns += other.turns;
        self.reported_turns += other.reported_turns;
        if other.turns > 0 && had_turns {
            self.tokens.total_tokens = add(self.tokens.total_tokens, other.tokens.total_tokens);
            self.tokens.input_tokens = add(self.tokens.input_tokens, other.tokens.input_tokens);
            self.tokens.output_tokens = add(self.tokens.output_tokens, other.tokens.output_tokens);
            self.tokens.thought_tokens =
                add(self.tokens.thought_tokens, other.tokens.thought_tokens);
            self.tokens.cached_read_tokens = add(
                self.tokens.cached_read_tokens,
                other.tokens.cached_read_tokens,
            );
            self.tokens.cached_write_tokens = add(
                self.tokens.cached_write_tokens,
                other.tokens.cached_write_tokens,
            );
        } else if other.turns > 0 {
            self.tokens = other.tokens.clone();
        }
        self.estimated_cost = money(&self.estimated_cost, &other.estimated_cost);
        self.actual_session_cost = money(&self.actual_session_cost, &other.actual_session_cost);
        // Context-window gauges refer to different model sessions and cannot be added.
        self.context_used = None;
        self.context_size = None;
    }
}

fn sum_known(mut values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    values.try_fold(0u64, |sum, value| sum.checked_add(value?))
}

impl Ledger {
    pub fn record(&mut self, turn: Turn) -> bool {
        // Keys survive reconnect and restart, so retransmission cannot charge a second turn.
        if self.turns.iter().any(|old| old.id == turn.id) {
            return false;
        }
        self.turns.push(turn);
        true
    }

    pub fn summary(&self) -> Summary {
        if self.turns.is_empty() {
            return Summary {
                actual_session_cost: self.actual_session_cost.clone(),
                context_used: self.context_used,
                context_size: self.context_size,
                ..Summary::default()
            };
        }
        let reported = self
            .turns
            .iter()
            .filter(|turn| turn.tokens.is_some())
            .count();
        let field = |get: fn(&Tokens) -> Option<u64>| {
            sum_known(
                self.turns
                    .iter()
                    .map(|turn| turn.tokens.as_ref().and_then(get)),
            )
        };
        // A missing category in any turn makes the aggregate unknown, not zero.
        let tokens = Tokens {
            total_tokens: field(|tokens| tokens.total_tokens),
            input_tokens: field(|tokens| tokens.input_tokens),
            output_tokens: field(|tokens| tokens.output_tokens),
            thought_tokens: field(|tokens| tokens.thought_tokens),
            cached_read_tokens: field(|tokens| tokens.cached_read_tokens),
            cached_write_tokens: field(|tokens| tokens.cached_write_tokens),
        };
        let estimated_cost = self
            .turns
            .iter()
            .try_fold(0.0, |sum, turn| Some(sum + turn.estimate.as_ref()?.total?))
            .and_then(|amount| Money::valid(amount, "USD"));
        Summary {
            turns: self.turns.len(),
            reported_turns: reported,
            tokens,
            estimated_cost,
            actual_session_cost: self.actual_session_cost.clone(),
            context_used: self.context_used,
            context_size: self.context_size,
        }
    }

    pub fn report_cost(&mut self, used: u64, size: u64, cost: Option<Money>) {
        self.context_used = Some(used);
        self.context_size = Some(size);
        if let Some(cost) = cost {
            self.actual_session_cost = Some(cost);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreported_turn_keeps_totals_unknown_and_a_retried_key_does_not_count_twice() {
        let mut ledger = Ledger::default();
        let turn = Turn {
            id: "key".into(),
            agent: "agent".into(),
            timestamp: 1,
            tokens: None,
            estimate: None,
        };
        assert!(ledger.record(turn.clone()));
        assert!(!ledger.record(turn));
        assert_eq!(ledger.summary().turns, 1);
        assert_eq!(ledger.summary().tokens.total_tokens, None);
    }

    #[test]
    fn reported_totals_do_not_add_reasoning_or_cached_tokens_again() {
        let mut ledger = Ledger::default();
        ledger.record(Turn {
            id: "one".into(),
            agent: "agent".into(),
            timestamp: 1,
            tokens: Some(Tokens {
                total_tokens: Some(11),
                input_tokens: Some(6),
                output_tokens: Some(4),
                thought_tokens: Some(3),
                cached_read_tokens: Some(2),
                ..Tokens::default()
            }),
            estimate: None,
        });
        assert_eq!(ledger.summary().tokens.total_tokens, Some(11));
        assert_eq!(ledger.summary().tokens.cached_write_tokens, None);
    }

    #[test]
    fn an_agent_total_combines_sessions_without_turns_while_preserving_unknown_costs() {
        let mut first = Ledger::default();
        first.record(Turn {
            id: "one".into(),
            agent: "agent".into(),
            timestamp: 1,
            tokens: Some(Tokens {
                total_tokens: Some(11),
                ..Tokens::default()
            }),
            estimate: None,
        });
        let mut second = Ledger::default();
        second.record(Turn {
            id: "one".into(),
            agent: "agent".into(),
            timestamp: 2,
            tokens: Some(Tokens {
                total_tokens: Some(12),
                ..Tokens::default()
            }),
            estimate: None,
        });
        let mut aggregate = Ledger::default().summary();
        aggregate.combine(&first.summary());
        aggregate.combine(&second.summary());
        assert_eq!(aggregate.turns, 2);
        assert_eq!(aggregate.tokens.total_tokens, Some(23));
        assert!(aggregate.estimated_cost.is_none());
    }
}
