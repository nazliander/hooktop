//! What tokens cost, at Anthropic's API list prices.
//!
//! Prices are USD per million tokens, from
//! <https://platform.claude.com/docs/en/about-claude/pricing> (checked
//! November 2026). They're what the same usage would cost on the API: on a
//! Claude subscription you don't pay per token. Fast mode and US data
//! residency (both recorded per call in the transcript) are priced; batch
//! discounts don't apply to Claude Code. Unknown or newer models can be
//! priced with a `prices.json` override (see [`Pricing::with_overrides`]).

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use crate::usage::Usage;

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

impl Price {
    /// A price from input and output alone, with the standard cache
    /// multipliers (writes 1.25× and 2×, reads 0.1×).
    pub fn standard(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_write_5m: input * 1.25,
            cache_write_1h: input * 2.0,
            cache_read: input * 0.1,
        }
    }

    const fn full(input: f64, cache_write_5m: f64, cache_write_1h: f64, cache_read: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_write_5m,
            cache_write_1h,
            cache_read,
        }
    }

    pub fn cost(&self, u: &Usage) -> f64 {
        (u.input as f64 * self.input
            + u.output as f64 * self.output
            + u.cache_write_5m as f64 * self.cache_write_5m
            + u.cache_write_1h as f64 * self.cache_write_1h
            + u.cache_read as f64 * self.cache_read)
            / 1_000_000.0
    }
}

/// Fast mode's input and output prices; cache prices scale with them.
const FAST: &[(&str, f64, f64)] = &[
    ("opus-5-5", 8.0, 40.0),
    ("opus-5", 10.0, 50.0),
    ("opus-4-8", 10.0, 50.0),
];

/// US-only data residency (`inference_geo: "us"`) multiplies every price.
const US_RESIDENCY: f64 = 1.1;

/// Haiku 5.5 charges more for a request whose prompt (all input, cache
/// reads and writes included) is over this many tokens.
const HAIKU_5_5_TIER: u64 = 100_000;

/// The official table: base input, 5m write, 1h write, cache hit, output.
const BUILTIN: &[(&str, Price)] = &[
    ("fable-5-1", Price::full(10.0, 12.5, 20.0, 0.25, 50.0)),
    ("opus-5-5", Price::full(4.0, 5.0, 8.0, 0.20, 20.0)),
    ("sonnet-5-5", Price::full(2.0, 2.5, 4.0, 0.10, 10.0)),
    ("haiku-5-5", Price::full(0.10, 0.125, 0.20, 0.01, 0.50)),
    ("haiku-5-5>100k", Price::full(0.50, 0.625, 1.0, 0.05, 2.50)),
    ("mythos-5-1", Price::full(10.0, 12.5, 20.0, 0.25, 50.0)),
    ("fable-5", Price::full(10.0, 12.5, 20.0, 1.0, 50.0)),
    ("mythos-5", Price::full(10.0, 12.5, 20.0, 1.0, 50.0)),
    ("opus-5", Price::full(5.0, 6.25, 10.0, 0.50, 25.0)),
    ("opus-4-8", Price::full(5.0, 6.25, 10.0, 0.50, 25.0)),
    ("opus-4-7", Price::full(5.0, 6.25, 10.0, 0.50, 25.0)),
    ("opus-4-6", Price::full(5.0, 6.25, 10.0, 0.50, 25.0)),
    ("opus-4-5", Price::full(5.0, 6.25, 10.0, 0.50, 25.0)),
    ("opus-4-1", Price::full(15.0, 18.75, 30.0, 1.50, 75.0)),
    ("opus-4", Price::full(15.0, 18.75, 30.0, 1.50, 75.0)),
    ("sonnet-5", Price::full(2.0, 2.5, 4.0, 0.20, 10.0)),
    ("sonnet-4-6", Price::full(3.0, 3.75, 6.0, 0.30, 15.0)),
    ("sonnet-4-5", Price::full(3.0, 3.75, 6.0, 0.30, 15.0)),
    ("sonnet-4", Price::full(3.0, 3.75, 6.0, 0.30, 15.0)),
    ("haiku-4-5", Price::full(1.0, 1.25, 2.0, 0.10, 5.0)),
    ("3-5-haiku", Price::full(0.80, 1.0, 1.60, 0.08, 4.0)),
];

/// `claude-opus-5-5`, `claude-sonnet-4-5-20250929`,
/// `anthropic.claude-opus-4-1-20250805-v1:0`, `claude-opus-5-5[1m]` →
/// `opus-5-5`, `sonnet-4-5`, `opus-4-1`, `opus-5-5`.
pub fn model_key(model: &str) -> String {
    let mut s = model.trim().to_ascii_lowercase();
    if let Some(i) = s.find('[') {
        s.truncate(i);
    }
    if let Some(i) = s.rfind('/') {
        s = s[i + 1..].to_string();
    }
    // Bedrock ids, with or without a region prefix: `us.anthropic.claude-…`.
    if let Some(i) = s.rfind("anthropic.") {
        s = s[i + "anthropic.".len()..].to_string();
    }
    if let Some(i) = s.find("-v") {
        // Bedrock's `-v1:0`.
        if s[i + 2..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            s.truncate(i);
        }
    }
    if let Some(i) = s.find('@') {
        // Vertex's `@20250929`.
        s.truncate(i);
    }
    if let Some(rest) = s.strip_prefix("claude-") {
        s = rest.to_string();
    }
    // A trailing date: `-20250929`.
    if let Some((head, tail)) = s.rsplit_once('-')
        && tail.len() == 8
        && tail.chars().all(|c| c.is_ascii_digit())
    {
        s = head.to_string();
    }
    s
}

#[derive(Debug, Clone)]
pub struct Pricing {
    table: HashMap<String, Price>,
}

impl Default for Pricing {
    fn default() -> Self {
        Self::builtin()
    }
}

#[derive(Deserialize)]
struct OverrideEntry {
    input: f64,
    output: f64,
    cache_write_5m: Option<f64>,
    cache_write_1h: Option<f64>,
    cache_read: Option<f64>,
}

impl Pricing {
    pub fn builtin() -> Self {
        Self {
            table: BUILTIN.iter().map(|(k, p)| ((*k).to_string(), *p)).collect(),
        }
    }

    /// Add or replace prices from JSON like
    /// `{"claude-opus-6": {"input": 4, "output": 20}}`. Missing cache prices
    /// follow the standard multipliers. Returns the keys it couldn't read.
    pub fn with_overrides(mut self, json: &Value) -> (Self, Vec<String>) {
        let mut bad = Vec::new();
        if let Some(map) = json.as_object() {
            for (model, v) in map {
                match serde_json::from_value::<OverrideEntry>(v.clone()) {
                    Ok(e) => {
                        let base = Price::standard(e.input, e.output);
                        self.table.insert(
                            model_key(model),
                            Price {
                                cache_write_5m: e.cache_write_5m.unwrap_or(base.cache_write_5m),
                                cache_write_1h: e.cache_write_1h.unwrap_or(base.cache_write_1h),
                                cache_read: e.cache_read.unwrap_or(base.cache_read),
                                ..base
                            },
                        );
                    }
                    Err(_) => bad.push(model.clone()),
                }
            }
        }
        (self, bad)
    }

    /// The price of one request to `model` with `prompt_tokens` of input.
    pub fn price(&self, model: &str, prompt_tokens: u64) -> Option<Price> {
        let key = model_key(model);
        if key == "haiku-5-5"
            && prompt_tokens > HAIKU_5_5_TIER
            && let Some(p) = self.table.get("haiku-5-5>100k")
        {
            return Some(*p);
        }
        self.table.get(&key).copied()
    }

    /// The cost of one model call, or `None` for a model without a price.
    pub fn cost(&self, u: &Usage) -> Option<f64> {
        let mut p = self.price(&u.model, u.prompt_tokens())?;
        if u.speed.as_deref() == Some("fast")
            && let Some((_, input, output)) = FAST.iter().find(|(k, _, _)| *k == model_key(&u.model))
        {
            // Cache prices keep their ratio to input (model-specific).
            let r = input / p.input;
            p = Price {
                input: *input,
                output: *output,
                cache_write_5m: p.cache_write_5m * r,
                cache_write_1h: p.cache_write_1h * r,
                cache_read: p.cache_read * r,
            };
        }
        let mut cost = p.cost(u);
        if u.geo.as_deref() == Some("us") {
            cost *= US_RESIDENCY;
        }
        Some(cost)
    }
}

/// `$12.34`, `$0.42`, `$0.008`, `<$0.001`.
pub fn format_usd(v: f64) -> String {
    if v <= 0.0 {
        "$0".into()
    } else if v >= 100.0 {
        format!("${v:.0}")
    } else if v >= 0.01 {
        format!("${v:.2}")
    } else if v >= 0.001 {
        format!("${v:.3}")
    } else {
        "<$0.001".into()
    }
}

/// `950`, `12.3k`, `4.1M`.
pub fn format_tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(model: &str, input: u64, output: u64, read: u64, w5: u64, w1: u64) -> Usage {
        Usage {
            message_id: "m".into(),
            model: model.into(),
            input,
            output,
            cache_read: read,
            cache_write_5m: w5,
            cache_write_1h: w1,
            ..Default::default()
        }
    }

    #[test]
    fn model_ids_normalize() {
        for (id, key) in [
            ("claude-opus-5-5", "opus-5-5"),
            ("claude-opus-5", "opus-5"),
            ("claude-sonnet-4-5-20250929", "sonnet-4-5"),
            ("claude-opus-4-20250514", "opus-4"),
            ("claude-3-5-haiku-20241022", "3-5-haiku"),
            ("anthropic.claude-opus-4-1-20250805-v1:0", "opus-4-1"),
            ("claude-sonnet-4-5@20250929", "sonnet-4-5"),
            ("claude-opus-5-5[1m]", "opus-5-5"),
            ("us.anthropic.claude-haiku-4-5-20251001-v1:0", "haiku-4-5"),
        ] {
            assert_eq!(model_key(id), key, "{id}");
        }
    }

    #[test]
    fn prices_match_the_official_table() {
        let p = Pricing::builtin();
        // Opus 5.5: $4 in, $20 out, $5 5m write, $8 1h write, $0.20 read.
        let u = usage("claude-opus-5-5", 1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert!((p.cost(&u).unwrap() - (4.0 + 20.0 + 0.20 + 5.0 + 8.0)).abs() < 1e-9);
        // Opus 5 is a different price from Opus 5.5.
        let u = usage("claude-opus-5", 1_000_000, 0, 0, 0, 0);
        assert!((p.cost(&u).unwrap() - 5.0).abs() < 1e-9);
        // Haiku 5.5's tier is per request, on the whole prompt.
        let small = usage("claude-haiku-5-5", 50_000, 1_000_000, 50_000, 0, 0);
        assert!((p.cost(&small).unwrap() - (0.005 + 0.5 + 0.0005)).abs() < 1e-9);
        let big = usage("claude-haiku-5-5", 50_000, 1_000_000, 60_000, 0, 0);
        assert!((p.cost(&big).unwrap() - (0.025 + 2.5 + 0.003)).abs() < 1e-9);
        assert_eq!(p.cost(&usage("claude-unknown-9", 10, 10, 0, 0, 0)), None);
        // Fast mode: Opus 5.5 at $8 / $40, cache read stays 0.05× input.
        let mut f = usage("claude-opus-5-5", 1_000_000, 1_000_000, 1_000_000, 0, 0);
        f.speed = Some("fast".into());
        assert!((p.cost(&f).unwrap() - (8.0 + 40.0 + 0.40)).abs() < 1e-9);
        // US residency: 1.1× on top.
        let mut g = usage("claude-sonnet-5-5", 1_000_000, 0, 0, 0, 0);
        g.geo = Some("us".into());
        assert!((p.cost(&g).unwrap() - 2.2).abs() < 1e-9);
    }

    #[test]
    fn overrides_add_models() {
        let (p, bad) = Pricing::builtin().with_overrides(&json!({
            "claude-opus-6-20270101": {"input": 3, "output": 15},
            "broken": {"input": "free"}
        }));
        assert_eq!(bad, vec!["broken".to_string()]);
        let pr = p.price("claude-opus-6", 0).unwrap();
        assert_eq!(
            (pr.cache_write_5m, pr.cache_write_1h, pr.cache_read),
            (3.75, 6.0, 0.30000000000000004)
        );
    }

    #[test]
    fn formatting() {
        assert_eq!(format_usd(12.345), "$12.35");
        assert_eq!(format_usd(0.42), "$0.42");
        assert_eq!(format_usd(0.0081), "$0.008");
        assert_eq!(format_usd(0.0001), "<$0.001");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_345), "12.3k");
        assert_eq!(format_tokens(4_100_000), "4.1M");
    }
}
