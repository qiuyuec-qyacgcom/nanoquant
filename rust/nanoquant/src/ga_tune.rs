//! GA 离线参数搜索：只生成候选参数集，不激活、不自动应用。
//!
//! 适应度复用一个固定 walk-forward 回测：样本外平均收益 - 回撤惩罚 - 换手惩罚。
//! 回测失败（数据不足或配置非法）一律返回 `f64::NEG_INFINITY`，避免坏基因被误选。
//! 随机数固定 `seed`，保证同一输入与同一 seed 下结果可复现。

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value};

use crate::backtest_engine::{self, BasketMetrics};
use crate::history::HistoryBar;
use crate::regime::Regime;

pub(crate) type Genome = HashMap<String, f64>;

/// 单个可调基因：`step` 用于量化，`integer` 表示只允许整数取值。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gene {
    pub key: &'static str,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub integer: bool,
}

/// 默认基因集：只覆盖 AI 调参候选表允许的入场过滤参数，不碰杠杆/止损/方向。
pub(crate) fn default_genes() -> Vec<Gene> {
    vec![
        Gene {
            key: "entry_threshold",
            min: 0.01,
            max: 0.5,
            step: 0.01,
            integer: false,
        },
        Gene {
            key: "atr_pct_min",
            min: 0.0,
            max: 1.0,
            step: 0.01,
            integer: false,
        },
        Gene {
            key: "atr_pct_max",
            min: 6.0,
            max: 20.0,
            step: 0.1,
            integer: false,
        },
        Gene {
            key: "volume_ratio_min",
            min: 0.0,
            max: 1.0,
            step: 0.01,
            integer: false,
        },
        Gene {
            key: "funding_veto_bps",
            min: 0.0,
            max: 50.0,
            step: 0.5,
            integer: false,
        },
    ]
}

#[derive(Debug, Clone)]
pub(crate) struct GaConfig {
    pub enabled: bool,
    pub population: usize,
    pub generations: usize,
    pub mutation_rate: f64,
    pub seed: u64,
}

impl Default for GaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            population: 30,
            generations: 20,
            mutation_rate: 0.1,
            seed: 42,
        }
    }
}

impl GaConfig {
    /// 从 `ai_tuning` 配置组读取；缺键或越界一律回落到默认值并夹在注册范围内。
    pub(crate) fn from_ai_tuning(config: &Value) -> Self {
        let defaults = Self::default();
        let number = |key: &str| config.get(key).and_then(Value::as_f64);
        Self {
            enabled: config
                .get("ga_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(defaults.enabled),
            population: number("ga_population")
                .map(|value| value as usize)
                .unwrap_or(defaults.population)
                .clamp(10, 100),
            generations: number("ga_generations")
                .map(|value| value as usize)
                .unwrap_or(defaults.generations)
                .clamp(5, 100),
            mutation_rate: number("ga_mutation_rate")
                .filter(|value| value.is_finite())
                .unwrap_or(defaults.mutation_rate)
                .clamp(0.0, 1.0),
            seed: number("ga_seed")
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value.min(1_000_000.0) as u64)
                .unwrap_or(defaults.seed),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GaResult {
    pub best: Genome,
    pub best_fitness: f64,
    pub history: Vec<f64>,
}

const TOURNAMENT_K: usize = 3;
const ELITES: usize = 1;

/// 高斯随机数（Box-Muller），避免为一次变异引入额外依赖。
fn gaussian(rng: &mut StdRng) -> f64 {
    let u1: f64 = rng.random_range(f64::MIN_POSITIVE..1.0);
    let u2: f64 = rng.random_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// 夹到基因范围，并按 `step`/`integer` 量化；量化后再夹一次，防止浮点溢出边界。
fn clamp_gene(gene: &Gene, value: f64) -> f64 {
    let mut bounded = value.clamp(gene.min, gene.max);
    if gene.integer {
        bounded = bounded.round();
    } else if gene.step > 0.0 {
        let offset = ((bounded - gene.min) / gene.step).round();
        bounded = gene.min + offset * gene.step;
    }
    bounded.clamp(gene.min, gene.max)
}

fn random_genome(genes: &[Gene], rng: &mut StdRng) -> Genome {
    genes
        .iter()
        .map(|gene| {
            let raw = gene.min + rng.random::<f64>() * (gene.max - gene.min);
            (gene.key.to_string(), clamp_gene(gene, raw))
        })
        .collect()
}

/// 锦标赛选择：随机抽 `k` 个个体，取适应度最高者。
fn tournament<'a>(scored: &'a [(f64, Genome)], k: usize, rng: &mut StdRng) -> &'a Genome {
    let mut winner = &scored[rng.random_range(0..scored.len())];
    for _ in 1..k {
        let challenger = &scored[rng.random_range(0..scored.len())];
        if challenger.0 > winner.0 {
            winner = challenger;
        }
    }
    &winner.1
}

/// 单点交叉：按基因顺序切一刀，前半取自 `left`，后半取自 `right`。
fn crossover(left: &Genome, right: &Genome, genes: &[Gene], rng: &mut StdRng) -> Genome {
    if genes.len() < 2 {
        return left.clone();
    }
    let cut = rng.random_range(1..genes.len());
    genes
        .iter()
        .enumerate()
        .map(|(index, gene)| {
            let source = if index < cut { left } else { right };
            let value = source.get(gene.key).copied().unwrap_or(gene.min);
            (gene.key.to_string(), clamp_gene(gene, value))
        })
        .collect()
}

/// 高斯变异：每个基因以 `rate` 概率叠加 `0.1 * (max - min)` 的扰动。
fn mutate(genome: &mut Genome, genes: &[Gene], rate: f64, rng: &mut StdRng) {
    for gene in genes {
        if rng.random::<f64>() < rate {
            let current = genome.get(gene.key).copied().unwrap_or(gene.min);
            let sigma = (gene.max - gene.min) * 0.1;
            let next = current + gaussian(rng) * sigma;
            genome.insert(gene.key.to_string(), clamp_gene(gene, next));
        }
    }
}

/// 把基因写回完整 strategy 快照；其余键沿用基线，供证据门按全量参数回测。
pub(crate) fn params_with_genome(base: &Value, genome: &Genome) -> Value {
    let mut params = base.clone();
    if let Some(map) = params.as_object_mut() {
        for (key, value) in genome {
            map.insert(key.clone(), json!(*value));
        }
    }
    params
}

/// 默认回测周期对齐证据门白名单：`strategy.timeframe` 不在白名单时回退白名单首个，
/// 白名单为空则回退证据门默认周期（1h）。显式 `--interval` 始终优先。
pub(crate) fn resolve_timeframe(
    live_timeframe: &str,
    gate_timeframes: &[String],
    requested: Option<&str>,
) -> String {
    if let Some(frame) = requested.map(str::trim).filter(|frame| !frame.is_empty()) {
        return frame.to_string();
    }
    let evidence = gate_timeframes
        .iter()
        .map(|frame| frame.trim())
        .find(|frame| !frame.is_empty())
        .unwrap_or(crate::evidence_task::DEFAULT_TIMEFRAME);
    if live_timeframe.trim() != evidence {
        tracing::warn!(
            live = %live_timeframe,
            gate = %evidence,
            "strategy.timeframe 不在证据门白名单，GA 默认回退证据门周期"
        );
    }
    evidence.to_string()
}

/// 适应度：样本外平均收益 - 0.5 × 最大回撤 - 0.01 × 交易次数。
pub(crate) fn score_metrics(metrics: &BasketMetrics) -> f64 {
    metrics.oos_avg_return_pct
        - 0.5 * metrics.max_drawdown_pct
        - 0.01 * metrics.oos_trades as f64
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fitness(
    genome: &Genome,
    base_params: &Value,
    risk: &Value,
    basket: &[(String, Vec<HistoryBar>)],
    folds: i64,
    train_bars: i64,
    regime: Regime,
    timeframe: &str,
) -> f64 {
    let params = params_with_genome(base_params, genome);
    let config = match crate::evidence_task::build_config(&params, risk, timeframe) {
        Ok(config) => config,
        Err(error) => {
            tracing::debug!(%error, "GA 基因配置非法");
            return f64::NEG_INFINITY;
        }
    };
    let cancel = AtomicBool::new(false);
    match backtest_engine::run_walkforward_basket_fixed(
        basket,
        &config,
        folds,
        train_bars,
        regime,
        timeframe,
        &cancel,
    ) {
        Ok(metrics) => score_metrics(&metrics),
        Err(error) => {
            tracing::debug!(%error, "GA 基因回测失败");
            f64::NEG_INFINITY
        }
    }
}

/// 运行 GA：每代评估整个种群，记录当前最优适应度，精英保留 1 个后交叉/变异。
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_ga(
    genes: &[Gene],
    base_params: &Value,
    risk: &Value,
    basket: &[(String, Vec<HistoryBar>)],
    folds: i64,
    train_bars: i64,
    regime: Regime,
    timeframe: &str,
    cfg: &GaConfig,
) -> GaResult {
    let population = cfg.population.clamp(2, 100);
    let generations = cfg.generations.max(1);
    let rate = cfg.mutation_rate.clamp(0.0, 1.0);
    let mut rng = StdRng::seed_from_u64(cfg.seed);

    let mut current: Vec<Genome> = (0..population)
        .map(|_| random_genome(genes, &mut rng))
        .collect();
    let mut best = current[0].clone();
    let mut best_fitness = f64::NEG_INFINITY;
    let mut history = Vec::with_capacity(generations);

    for generation in 0..generations {
        let scored: Vec<(f64, Genome)> = current
            .iter()
            .map(|genome| {
                (
                    fitness(
                        genome,
                        base_params,
                        risk,
                        basket,
                        folds,
                        train_bars,
                        regime,
                        timeframe,
                    ),
                    genome.clone(),
                )
            })
            .collect();
        for (value, genome) in &scored {
            if *value > best_fitness {
                best_fitness = *value;
                best = genome.clone();
            }
        }
        history.push(best_fitness);

        if generation + 1 == generations {
            break;
        }

        let mut ranked = scored;
        ranked.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut next = Vec::with_capacity(population);
        for (_, genome) in ranked.iter().take(ELITES.min(population)) {
            next.push(genome.clone());
        }
        while next.len() < population {
            let left = tournament(&ranked, TOURNAMENT_K, &mut rng);
            let right = tournament(&ranked, TOURNAMENT_K, &mut rng);
            let mut child = crossover(left, right, genes, &mut rng);
            mutate(&mut child, genes, rate, &mut rng);
            next.push(child);
        }
        current = next;
    }

    GaResult {
        best,
        best_fitness,
        history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_in_bounds(genome: &Genome, genes: &[Gene]) {
        for gene in genes {
            let value = genome[gene.key];
            assert!(
                value >= gene.min - 1e-9 && value <= gene.max + 1e-9,
                "{} = {value} 越界 [{}, {}]",
                gene.key,
                gene.min,
                gene.max
            );
            assert!(value.is_finite(), "{} 必须有限", gene.key);
        }
    }

    #[test]
    fn ga_default_interval_matches_evidence_gate() {
        let gate = vec!["1h".to_string()];
        assert_eq!(
            resolve_timeframe("1h", &gate, None),
            "1h",
            "与证据门一致时保持实盘周期"
        );
        assert_eq!(
            resolve_timeframe("4h", &gate, None),
            "1h",
            "strategy.timeframe=4h 不在白名单时必须回退证据门 1h"
        );
        assert_eq!(
            resolve_timeframe("4h", &gate, Some("4h")),
            "4h",
            "--interval 显式覆盖优先"
        );
        assert_eq!(
            resolve_timeframe("4h", &[], None),
            crate::evidence_task::DEFAULT_TIMEFRAME,
            "空白名单回退证据门默认周期"
        );
    }

    #[test]
    fn gene_bounds_are_respected() {
        let genes = default_genes();
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..200 {
            assert_in_bounds(&random_genome(&genes, &mut rng), &genes);
        }
    }

    #[test]
    fn crossover_keeps_genes_in_range() {
        let genes = default_genes();
        let mut rng = StdRng::seed_from_u64(11);
        let left = random_genome(&genes, &mut rng);
        let right = random_genome(&genes, &mut rng);
        for _ in 0..200 {
            assert_in_bounds(&crossover(&left, &right, &genes, &mut rng), &genes);
        }
    }

    #[test]
    fn mutation_stays_within_bounds() {
        let genes = default_genes();
        let mut rng = StdRng::seed_from_u64(13);
        let mut genome = random_genome(&genes, &mut rng);
        for _ in 0..500 {
            mutate(&mut genome, &genes, 1.0, &mut rng);
            assert_in_bounds(&genome, &genes);
        }
    }

    #[test]
    fn ga_is_deterministic_with_seed() {
        let cfg = GaConfig {
            enabled: true,
            population: 6,
            generations: 3,
            mutation_rate: 0.2,
            seed: 42,
        };
        let params = json!({"timeframe": "1h", "entry_threshold": 0.3});
        let risk = json!({});
        let basket: Vec<(String, Vec<HistoryBar>)> = Vec::new();
        let regime = Regime::from_str("range").unwrap();
        let genes = default_genes();
        let first = run_ga(
            &genes, &params, &risk, &basket, 3, 800, regime, "1h", &cfg,
        );
        let second = run_ga(
            &genes, &params, &risk, &basket, 3, 800, regime, "1h", &cfg,
        );
        assert_eq!(first.best, second.best);
        assert_eq!(first.best_fitness, second.best_fitness);
        assert_eq!(first.history, second.history);
        assert_eq!(first.history.len(), 3);
    }

    #[test]
    fn fitness_rejects_failed_backtest() {
        let genome: Genome = default_genes()
            .iter()
            .map(|gene| (gene.key.to_string(), gene.min))
            .collect();
        let params = params_with_genome(&json!({"timeframe": "1h"}), &genome);
        let value = fitness(
            &genome,
            &params,
            &json!({}),
            &[],
            3,
            800,
            Regime::from_str("range").unwrap(),
            "1h",
        );
        assert!(
            value.is_infinite() && value.is_sign_negative(),
            "回测失败必须返回 NEG_INFINITY，实际 {value}"
        );
        let result = run_ga(
            &default_genes(),
            &params,
            &json!({}),
            &[],
            3,
            800,
            Regime::from_str("range").unwrap(),
            "1h",
            &GaConfig {
                enabled: true,
                population: 3,
                generations: 1,
                mutation_rate: 0.1,
                seed: 1,
            },
        );
        assert!(result.best_fitness.is_infinite() && result.best_fitness.is_sign_negative());
    }

    #[test]
    fn config_reads_ga_enabled_and_defaults_to_disabled() {
        assert!(!GaConfig::from_ai_tuning(&json!({})).enabled);
        assert!(GaConfig::from_ai_tuning(&json!({"ga_enabled": true})).enabled);
        assert!(!GaConfig::from_ai_tuning(&json!({"ga_enabled": false})).enabled);
    }
}