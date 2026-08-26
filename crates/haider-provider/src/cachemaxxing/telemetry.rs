use haider_protocol::provider::NormalizedUsage;

/// Benchmark scenario labels. They are deliberately orthogonal to request
/// kind so warm/rewarm/keepalive traffic cannot disappear from a scenario's
/// denominator when lifecycle execution is enabled later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CacheScenario {
    Cold,
    Warm,
    IdleExpiry,
    Resume,
    ToolLoop,
    ReasoningLoop,
    Compaction,
    SchemaChange,
    ParallelAgent,
}

/// One raw provider request included in an economic cache benchmark.
#[derive(Debug, Clone, Copy)]
pub struct CacheEconomicSample<'a> {
    pub scenario: CacheScenario,
    pub usage: &'a NormalizedUsage,
}

/// Economic hit rate: cache reads divided by all provider-reported input
/// classes. `logical_input` is already `input + read + write` for separate-
/// counter providers and total input for subset-counter providers, so using it
/// avoids double-counting writes while exactly matching the raw formula.
///
/// Returns `None` unless every included request has authoritative cache
/// telemetry. Callers must pass warm, rewarm, and keepalive samples just like
/// ordinary turns; this function has no request-kind exclusion.
#[must_use]
pub fn economic_cache_hit_rate(samples: &[CacheEconomicSample<'_>]) -> Option<f64> {
    let mut read = 0_u64;
    let mut denominator = 0_u64;
    for sample in samples {
        let usage = sample.usage;
        if usage.cache_telemetry_input != usage.logical_input {
            return None;
        }
        read = read.saturating_add(usage.cache_read_input);
        denominator = denominator.saturating_add(usage.logical_input);
    }
    if denominator == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some(read as f64 / denominator as f64)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use haider_protocol::provider::CacheStatAvailability;

    use super::*;

    fn usage(input: u64, read: u64, write: u64) -> NormalizedUsage {
        let logical = input.saturating_add(read).saturating_add(write);
        NormalizedUsage {
            logical_input: logical,
            uncached_input: input.saturating_add(write),
            cache_read_input: read,
            cache_write_input: write,
            cache_status: CacheStatAvailability::Present,
            cache_write_status: CacheStatAvailability::Present,
            cache_telemetry_input: logical,
            ..NormalizedUsage::default()
        }
    }

    #[test]
    fn lifecycle_writes_and_reads_stay_in_the_denominator() {
        let cold = usage(100, 0, 50);
        let warm = usage(0, 100, 0);
        let keepalive = usage(0, 20, 0);
        let samples = [
            CacheEconomicSample {
                scenario: CacheScenario::Cold,
                usage: &cold,
            },
            CacheEconomicSample {
                scenario: CacheScenario::Warm,
                usage: &warm,
            },
            CacheEconomicSample {
                scenario: CacheScenario::IdleExpiry,
                usage: &keepalive,
            },
        ];
        assert_eq!(economic_cache_hit_rate(&samples), Some(120.0 / 270.0));
    }

    #[test]
    fn incomplete_provider_telemetry_is_not_a_zero() {
        let unavailable = NormalizedUsage {
            logical_input: 100,
            ..NormalizedUsage::default()
        };
        assert_eq!(
            economic_cache_hit_rate(&[CacheEconomicSample {
                scenario: CacheScenario::Resume,
                usage: &unavailable,
            }]),
            None
        );
    }
}
