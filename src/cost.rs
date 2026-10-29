//! Live cost estimate. No IO.
//!
//! claude reports the real cost only in `result.total_cost_usd`, at the end of a turn. Turns can run for many
//! minutes (observed: 12–20), so a cap checked against results alone was crossed 10+ minutes before anyone
//! noticed. Each assistant message carries its token usage as it starts; the meter prices those tokens with a
//! rate calibrated against every completed turn, so the total moves with every API call.

use serde_json::Value;

/// Token usage of one API call or one turn.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    pub input: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
    pub output: u64,
}

/// Relative prices, input = 1. Fitted on a real run (claude-opus-5-5: input $4, 1h cache write $8, cache read
/// $0.20, output $20 per MTok; exact on all 50 turns). Only the ratios matter; the scale is calibrated.
const W_5M: f64 = 1.25;
const W_1H: f64 = 2.0;
const W_READ: f64 = 0.05;
const W_OUT: f64 = 5.0;

/// Dollars per input token before the first turn has been calibrated. The highest list price in use, so an
/// uncalibrated estimate errs high, which is the safe side for a budget cap.
const DEFAULT_USD_PER_UNIT: f64 = 4e-6;
/// Total weighted tokens per input-side weighted token before calibration. In-flight messages report only
/// their starting output tokens, so output is extrapolated with this factor (observed: about 1.3).
const DEFAULT_OUT_FACTOR: f64 = 1.3;

impl Usage {
    /// Reads claude's `usage` object. When the cache-write breakdown is missing, writes are priced as 1h
    /// writes, the more expensive kind.
    pub fn from_json(u: &Value) -> Usage {
        let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
        let total_write = n(u.get("cache_creation_input_tokens"));
        let (w5, w1h) = match u.get("cache_creation") {
            Some(c) => (
                n(c.get("ephemeral_5m_input_tokens")),
                n(c.get("ephemeral_1h_input_tokens")),
            ),
            None => (0, total_write),
        };
        Usage {
            input: n(u.get("input_tokens")),
            cache_write_5m: w5,
            cache_write_1h: w1h,
            cache_read: n(u.get("cache_read_input_tokens")),
            output: n(u.get("output_tokens")),
        }
    }

    /// Weighted tokens on the input side (everything but output).
    pub fn input_units(&self) -> f64 {
        self.input as f64
            + self.cache_write_5m as f64 * W_5M
            + self.cache_write_1h as f64 * W_1H
            + self.cache_read as f64 * W_READ
    }

    pub fn units(&self) -> f64 {
        self.input_units() + self.output as f64 * W_OUT
    }
}

/// Calibration shared by all workers (they run the same model).
#[derive(Debug, Clone, Default)]
pub struct Meter {
    cost: f64,
    units: f64,
    input_units: f64,
}

impl Meter {
    /// Records a completed turn: its real cost and its exact usage.
    pub fn calibrate(&mut self, turn_cost: f64, turn: &Usage) {
        if turn_cost <= 0.0 || turn.units() <= 0.0 {
            return;
        }
        self.cost += turn_cost;
        self.units += turn.units();
        self.input_units += turn.input_units();
    }

    pub fn usd_per_unit(&self) -> f64 {
        if self.units > 0.0 {
            self.cost / self.units
        } else {
            DEFAULT_USD_PER_UNIT
        }
    }

    pub fn out_factor(&self) -> f64 {
        if self.input_units > 0.0 {
            self.units / self.input_units
        } else {
            DEFAULT_OUT_FACTOR
        }
    }

    /// Estimated dollars for in-flight calls whose input-side weighted tokens sum to `input_units`.
    pub fn estimate(&self, input_units: f64) -> f64 {
        input_units * self.out_factor() * self.usd_per_unit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_usage_with_and_without_the_write_breakdown() {
        let u = Usage::from_json(&json!({
            "input_tokens": 4, "cache_creation_input_tokens": 147, "cache_read_input_tokens": 60567,
            "output_tokens": 3, "cache_creation": { "ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 147 }
        }));
        assert_eq!(
            u,
            Usage {
                input: 4,
                cache_write_5m: 0,
                cache_write_1h: 147,
                cache_read: 60567,
                output: 3
            }
        );
        let flat = Usage::from_json(&json!({ "cache_creation_input_tokens": 10 }));
        assert_eq!(flat.cache_write_1h, 10, "unknown writes are priced as 1h");
    }

    #[test]
    fn weights_reproduce_opus_prices() {
        // One real turn: 209 input, 310,297 1h writes, 19,076,330 reads, 131,901 output → $8.936498.
        let turn = Usage {
            input: 209,
            cache_write_5m: 0,
            cache_write_1h: 310_297,
            cache_read: 19_076_330,
            output: 131_901,
        };
        assert!(
            (turn.units() * 4e-6 - 8.936498).abs() < 1e-6,
            "{}",
            turn.units() * 4e-6
        );
    }

    #[test]
    fn calibration_recovers_the_price_and_output_share() {
        let mut m = Meter::default();
        assert_eq!(m.usd_per_unit(), DEFAULT_USD_PER_UNIT);
        let turn = Usage {
            input: 1000,
            cache_read: 100_000,
            output: 2000,
            ..Default::default()
        };
        // A model at a quarter of the default price.
        m.calibrate(turn.units() * 1e-6, &turn);
        assert!((m.usd_per_unit() - 1e-6).abs() < 1e-12);
        assert!((m.out_factor() - turn.units() / turn.input_units()).abs() < 1e-9);
        let in_flight = Usage {
            cache_read: 100_000,
            input: 1000,
            ..Default::default()
        };
        let est = m.estimate(in_flight.input_units());
        assert!((est - turn.units() * 1e-6).abs() < 1e-9, "{est}");
    }

    #[test]
    fn uncalibrated_estimate_errs_high() {
        let call = Usage {
            cache_read: 200_000,
            ..Default::default()
        };
        assert!(Meter::default().estimate(call.input_units()) > 200_000.0 * 0.05 * 4e-6);
    }
}
