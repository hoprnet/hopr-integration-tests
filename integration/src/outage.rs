//! Relay outages for the scenarios: which return relays go down, how, for how long, and at what
//! data rate.
//!
//! A relay is taken down by pausing its `hoprd` process (SIGSTOP) and brought back by resuming it
//! (SIGCONT). To the rest of the network a paused relay is unreachable, as if it had crashed, but
//! its process survives, so a later run in the same cluster gets it back.
//!
//! Each scenario has its own default outage. Four environment variables override it, so a sweep
//! needs no rebuild:
//!
//! | Variable | Values | Meaning |
//! | -------- | ------ | ------- |
//! | `SURB_RELAY_OUTAGE` | `none`, `down`, `<down_s>/<up_s>` | no outage; relays down for the whole outage; or intermittent: down for `down_s`, up for `up_s`, repeated |
//! | `SURB_RELAYS_DOWN` | `all-but-one`, `all`, `<n>` | all return relays but one (S5 keeps the busiest, others the least used); every one; or the `n` most used |
//! | `SURB_LOAD_MBPS` | MB/s | data rate during the outage |
//! | `SURB_OUTAGE_SECS` | seconds | how long the outage lasts |

use std::time::Duration;

use crate::{Address, cluster::NodeInfo, relayers::RelayerSpread};

/// How the chosen relays fail.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pattern {
    /// Down for the whole outage.
    Down,
    /// Down for `down`, then up for `up`, repeated until the outage ends.
    Intermittent { down: Duration, up: Duration },
}

/// Which return relays go down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RelaysDown {
    /// All but the least used one. The remaining relay keeps the exit answering, which the
    /// return-path detector needs before it calls the others dead.
    AllButOne,
    /// Every return relay. Nothing answers, so the detector stays silent and only the 15 s
    /// buffer-estimate correction reacts.
    All,
    /// The `n` most used.
    MostUsed(usize),
}

/// One outage: which relays, how they fail, for how long, under what load.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutagePlan {
    pub pattern: Pattern,
    pub relays: RelaysDown,
    /// Data rate offered during the outage, in MB/s.
    pub load_mbps: f64,
    pub duration: Duration,
}

impl OutagePlan {
    /// The outage to run: `default` unless the environment overrides it. `template` supplies the
    /// fields a scenario without a default outage (`default = None`) uses once any variable is set.
    /// `None` means no outage.
    pub fn configure(default: Option<Self>, template: Self) -> anyhow::Result<Option<Self>> {
        Self::configure_from(default, template, |key| std::env::var(key).ok())
    }

    fn configure_from(
        default: Option<Self>,
        template: Self,
        var: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        let var = |key: &str| {
            var(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let outage = var("SURB_RELAY_OUTAGE");
        let relays = var("SURB_RELAYS_DOWN");
        let load = var("SURB_LOAD_MBPS");
        let secs = var("SURB_OUTAGE_SECS");
        if outage.is_none() && relays.is_none() && load.is_none() && secs.is_none() {
            return Ok(default);
        }
        if outage.as_deref() == Some("none") {
            return Ok(None);
        }
        let mut plan = default.unwrap_or(template);
        if let Some(v) = secs {
            plan.duration = Duration::from_secs_f64(
                v.parse()
                    .map_err(|_| anyhow::anyhow!("SURB_OUTAGE_SECS={v:?}: expected seconds"))?,
            );
        }
        if let Some(v) = outage {
            plan.pattern = parse_pattern(&v)?;
        }
        if let Some(v) = relays {
            plan.relays = parse_relays(&v)?;
        }
        if let Some(v) = load {
            plan.load_mbps = v
                .parse()
                .map_err(|_| anyhow::anyhow!("SURB_LOAD_MBPS={v:?}: expected MB/s"))?;
        }
        Ok(Some(plan))
    }

    /// One line for the run log.
    pub fn describe(&self) -> String {
        let relays = match self.relays {
            RelaysDown::AllButOne => "all return relays but one".to_string(),
            RelaysDown::All => "all return relays".to_string(),
            RelaysDown::MostUsed(n) => format!("the {n} most used return relays"),
        };
        let pattern = match self.pattern {
            Pattern::Down => "down".to_string(),
            Pattern::Intermittent { down, up } => format!(
                "down {:.1} s / up {:.1} s, repeated",
                down.as_secs_f64(),
                up.as_secs_f64()
            ),
        };
        format!(
            "{relays} {pattern} for {:.0} s at {} MB/s",
            self.duration.as_secs_f64(),
            self.load_mbps
        )
    }

    /// The longest time a relay stays down in one go.
    pub fn longest_down(&self) -> Duration {
        match self.pattern {
            Pattern::Down => self.duration,
            Pattern::Intermittent { down, .. } => down.min(self.duration),
        }
    }

    /// The relays to take down (most used first) and the addresses of those left up.
    pub fn pick(
        &self,
        candidates: &[NodeInfo],
        spread: &RelayerSpread,
    ) -> (Vec<NodeInfo>, Vec<Address>) {
        let used = |n: &NodeInfo| {
            spread
                .per_relayer
                .iter()
                .find(|(a, _)| *a == n.address)
                .map_or(0, |(_, c)| *c)
        };
        let mut ranked = candidates.to_vec();
        ranked.sort_by_key(|n| std::cmp::Reverse(used(n)));
        let take = match self.relays {
            RelaysDown::AllButOne => ranked.len().saturating_sub(1),
            RelaysDown::All => ranked.len(),
            RelaysDown::MostUsed(n) => n.min(ranked.len()),
        };
        let up = ranked[take..].iter().map(|n| n.address).collect();
        ranked.truncate(take);
        (ranked, up)
    }

    /// Take `relays` down according to the plan and bring them back when it ends. Returns the
    /// number of down periods. Callers keep a resume-on-drop guard in case this is cancelled.
    #[cfg(unix)]
    pub async fn run(&self, relays: &[NodeInfo]) -> anyhow::Result<u32> {
        let until = tokio::time::Instant::now() + self.duration;
        let (down, up) = match self.pattern {
            Pattern::Down => (self.duration, None),
            Pattern::Intermittent { down, up } => (down, Some(up)),
        };
        let mut periods = 0u32;
        while tokio::time::Instant::now() < until {
            for node in relays {
                node.pause()?;
            }
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(down.min(left)).await;
            for node in relays {
                node.resume()?;
            }
            periods += 1;
            match up {
                Some(up) => {
                    let left = until.saturating_duration_since(tokio::time::Instant::now());
                    tokio::time::sleep(up.min(left)).await;
                }
                None => break,
            }
        }
        Ok(periods)
    }
}

/// `down` or `<down_s>/<up_s>`.
pub fn parse_pattern(spec: &str) -> anyhow::Result<Pattern> {
    if spec == "down" {
        return Ok(Pattern::Down);
    }
    spec.split_once('/')
        .and_then(|(down, up)| {
            let down: f64 = down.trim().parse().ok()?;
            let up: f64 = up.trim().parse().ok()?;
            (down > 0.0 && up >= 0.0).then(|| Pattern::Intermittent {
                down: Duration::from_secs_f64(down),
                up: Duration::from_secs_f64(up),
            })
        })
        .ok_or_else(|| {
            anyhow::anyhow!("SURB_RELAY_OUTAGE={spec:?}: expected none, down or <down_s>/<up_s>")
        })
}

/// `all-but-one`, `all` or a count.
pub fn parse_relays(spec: &str) -> anyhow::Result<RelaysDown> {
    match spec {
        "all-but-one" => Ok(RelaysDown::AllButOne),
        "all" => Ok(RelaysDown::All),
        n => n.parse().map(RelaysDown::MostUsed).map_err(|_| {
            anyhow::anyhow!("SURB_RELAYS_DOWN={spec:?}: expected all-but-one, all or a count")
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: OutagePlan = OutagePlan {
        pattern: Pattern::Down,
        relays: RelaysDown::AllButOne,
        load_mbps: 0.25,
        duration: Duration::from_secs(30),
    };

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[tokio::test]
    async fn intermittent_outage_should_not_wait_out_an_up_period_past_its_deadline() {
        let plan = OutagePlan {
            pattern: Pattern::Intermittent {
                down: Duration::from_millis(10),
                up: Duration::from_secs(5),
            },
            duration: Duration::from_millis(30),
            ..TEMPLATE
        };
        // No processes needed: exercise the same scheduler with an empty relay set.
        let result = tokio::time::timeout(Duration::from_secs(1), plan.run(&[])).await;
        assert_eq!(
            result
                .expect("final up period exceeded the outage deadline")
                .unwrap(),
            1
        );
    }

    #[test]
    fn no_variables_should_keep_the_scenario_default() {
        let none = env(&[]);
        assert_eq!(
            OutagePlan::configure_from(None, TEMPLATE, &none).unwrap(),
            None
        );
        assert_eq!(
            OutagePlan::configure_from(Some(TEMPLATE), TEMPLATE, &none).unwrap(),
            Some(TEMPLATE)
        );
    }

    #[test]
    fn variables_should_override_field_by_field() {
        let vars = env(&[
            ("SURB_RELAY_OUTAGE", "6/2"),
            ("SURB_RELAYS_DOWN", "all"),
            ("SURB_LOAD_MBPS", "1"),
        ]);
        let plan = OutagePlan::configure_from(None, TEMPLATE, vars)
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.pattern,
            Pattern::Intermittent {
                down: Duration::from_secs(6),
                up: Duration::from_secs(2)
            }
        );
        assert_eq!(plan.relays, RelaysDown::All);
        assert_eq!(plan.load_mbps, 1.0);
        assert_eq!(plan.duration, TEMPLATE.duration, "not overridden");
    }

    #[test]
    fn none_should_disable_the_outage() {
        let vars = env(&[("SURB_RELAY_OUTAGE", "none")]);
        assert_eq!(
            OutagePlan::configure_from(Some(TEMPLATE), TEMPLATE, vars).unwrap(),
            None
        );
    }

    #[test]
    fn bad_values_should_be_rejected() {
        assert!(parse_pattern("6").is_err());
        assert!(
            parse_pattern("0/2").is_err(),
            "a zero down time is no outage"
        );
        assert!(parse_relays("most").is_err());
        assert_eq!(parse_relays("2").unwrap(), RelaysDown::MostUsed(2));
    }
}
