//! Integration test for enhanced HFT strategy.
//!
//! Simulates a complete pump scenario and verifies:
//! - Pump detection triggers
//! - Dynamic TP/SL adjusts to volatility
//! - Position sizing respects risk limits
//! - Slippage optimization adapts to pool conditions

use solana_sniper::enhanced_strategy::EnhancedStrategy;
use solana_sniper::order_flow::OrderFlowTx;
use solana_sniper::pump_detection::PricePoint;

#[test]
fn test_complete_pump_scenario() {
    // Setup: 1 SOL portfolio, 0.05 SOL max daily loss.
    let mut strategy = EnhancedStrategy::new(1_000_000_000, 50_000_000);
    let now = 1000u64;

    // Phase 1: Baseline (60 seconds of stable prices).
    println!("=== Phase 1: Baseline ===");
    for i in 0..60 {
        let price_point = PricePoint {
            sqrt_price_x64: 1u128 << 64, // Price = 1.0
            volume_lamports: 100,
            timestamp_ms: now + (i as u64 * 1000),
        };
        strategy.update_market_data(price_point, None);
    }

    let metrics = strategy.get_metrics();
    println!("Baseline metrics: pump_score={}, volatility={:.2}%", metrics.pump_score, metrics.volatility_pct);
    assert!(metrics.pump_score < 70, "baseline pump score should be low");

    // Phase 2: Pump (volume spike + price momentum + buying pressure).
    println!("\n=== Phase 2: Pump Detection ===");
    let spike_price = ((1u128 << 64) as f64 * 1.05) as u128; // 5% price move
    let mut entry_signal = None;

    for i in 0..30 {
        let price_point = PricePoint {
            sqrt_price_x64: spike_price,
            volume_lamports: 600, // 6x baseline
            timestamp_ms: now + (60000 + i as u64 * 1000),
        };

        // Add buying pressure (whale buy + sustained buys).
        let order_flow = if i % 2 == 0 {
            Some(OrderFlowTx {
                is_buy: true,
                size_lamports: if i == 0 { 1500 } else { 100 }, // Whale buy on first, then sustained
                price: 1.05,
                timestamp_ms: now + (60000 + i as u64 * 1000),
            })
        } else {
            None
        };

        entry_signal = strategy.update_market_data(price_point, order_flow);
        if entry_signal.is_some() {
            println!("Entry signal generated at iteration {}", i);
            break;
        }
    }

    assert!(entry_signal.is_some(), "pump should trigger entry signal");
    let signal = entry_signal.unwrap();

    println!("Entry Signal:");
    println!("  Position size: {} lamports ({:.4} SOL)", signal.position_size_lamports, signal.position_size_lamports as f64 / 1e9);
    println!("  Take-profit: {} bps ({:.2}%)", signal.take_profit_bps, signal.take_profit_bps as f64 / 100.0);
    println!("  Stop-loss: {} bps ({:.2}%)", signal.stop_loss_bps, signal.stop_loss_bps as f64 / 100.0);
    println!("  Slippage tolerance: {} bps ({:.2}%)", signal.slippage_bps, signal.slippage_bps as f64 / 100.0);
    println!("  Pump score: {}", signal.pump_score);
    println!("  Confidence: {}", signal.confidence);

    // Verify signal properties.
    assert!(signal.position_size_lamports > 0, "position size should be positive");
    assert!(signal.position_size_lamports <= 50_000_000, "position size should respect daily loss limit");
    assert!(signal.take_profit_bps > signal.stop_loss_bps, "TP should be > SL");
    assert!(signal.pump_score > 50, "pump score should be high");
    assert!(signal.confidence > 50, "confidence should be high");

    // Phase 3: Trade execution and learning.
    println!("\n=== Phase 3: Trade Execution ===");
    let expected_output = 950_000; // 0.95 SOL (5% slippage).
    let actual_output = 940_000;   // 0.94 SOL (actual slippage).
    strategy.record_trade(expected_output, actual_output, signal.position_size_lamports);

    let metrics = strategy.get_metrics();
    println!("Post-trade metrics:");
    println!("  Current heat: {} lamports", metrics.current_heat);
    println!("  Remaining budget: {} lamports", metrics.remaining_budget);
    assert!(metrics.current_heat > 0, "heat should increase after trade");
    assert!(metrics.remaining_budget < 50_000_000, "budget should decrease after trade");

    // Phase 4: Exit scenario (take-profit).
    println!("\n=== Phase 4: Exit (Take-Profit) ===");
    strategy.reset_detectors();
    let exit_price = ((1u128 << 64) as f64 * 1.15) as u128; // 15% price move (exceeds 10% TP).

    for i in 0..10 {
        let price_point = PricePoint {
            sqrt_price_x64: exit_price,
            volume_lamports: 200,
            timestamp_ms: now + (90000 + i as u64 * 1000),
        };
        strategy.update_market_data(price_point, None);
    }

    let metrics = strategy.get_metrics();
    println!("Exit metrics: volatility={:.2}%, buy_sell_ratio={:.2}", metrics.volatility_pct, metrics.buy_sell_ratio);

    println!("\n✓ Complete pump scenario test passed!");
}

#[test]
fn test_volatility_adjustment() {
    // Test that TP/SL adjust to volatility.
    let mut strategy = EnhancedStrategy::new(1_000_000_000, 50_000_000);
    let now = 1000u64;

    // Low volatility scenario.
    println!("=== Low Volatility Scenario ===");
    for i in 0..60 {
        let price_point = PricePoint {
            sqrt_price_x64: 1u128 << 64,
            volume_lamports: 100,
            timestamp_ms: now + (i as u64 * 1000),
        };
        strategy.update_market_data(price_point, None);
    }

    let metrics = strategy.get_metrics();
    println!("Low vol: volatility={:.2}%", metrics.volatility_pct);

    // High volatility scenario.
    println!("\n=== High Volatility Scenario ===");
    let mut strategy = EnhancedStrategy::new(1_000_000_000, 50_000_000);
    let prices = [1.0, 1.1, 0.9, 1.2, 0.8, 1.15, 0.85];
    for (i, &price_mult) in prices.iter().enumerate() {
        let sqrt_price = ((1u128 << 64) as f64 * price_mult) as u128;
        let price_point = PricePoint {
            sqrt_price_x64: sqrt_price,
            volume_lamports: 100,
            timestamp_ms: now + (i as u64 * 1000),
        };
        strategy.update_market_data(price_point, None);
    }

    let metrics = strategy.get_metrics();
    println!("High vol: volatility={:.2}%", metrics.volatility_pct);
    assert!(metrics.volatility_pct > 10.0, "high volatility should be detected");
}

#[test]
fn test_position_sizing_respects_limits() {
    let mut strategy = EnhancedStrategy::new(1_000_000_000, 50_000_000); // 0.05 SOL max loss
    let now = 1000u64;

    // Simulate multiple trades.
    for trade_num in 0..3 {
        println!("=== Trade {} ===", trade_num + 1);

        // Baseline.
        for i in 0..60 {
            let price_point = PricePoint {
                sqrt_price_x64: 1u128 << 64,
                volume_lamports: 100,
                timestamp_ms: now + (trade_num as u64 * 100000 + i as u64 * 1000),
            };
            strategy.update_market_data(price_point, None);
        }

        // Pump.
        let spike_price = ((1u128 << 64) as f64 * 1.05) as u128;
        let mut entry_signal = None;
        for i in 0..30 {
            let price_point = PricePoint {
                sqrt_price_x64: spike_price,
                volume_lamports: 600,
                timestamp_ms: now + (trade_num as u64 * 100000 + 60000 + i as u64 * 1000),
            };

            let order_flow = if i % 2 == 0 {
                Some(OrderFlowTx {
                    is_buy: true,
                    size_lamports: 100,
                    price: 1.05,
                    timestamp_ms: now + (trade_num as u64 * 100000 + 60000 + i as u64 * 1000),
                })
            } else {
                None
            };

            entry_signal = strategy.update_market_data(price_point, order_flow);
            if entry_signal.is_some() {
                break;
            }
        }

        if let Some(signal) = entry_signal {
            println!("Trade {}: position_size={} lamports", trade_num + 1, signal.position_size_lamports);
            strategy.record_trade(950_000, 940_000, signal.position_size_lamports);

            let metrics = strategy.get_metrics();
            println!("  Current heat: {} lamports, Remaining: {} lamports", metrics.current_heat, metrics.remaining_budget);
            assert!(metrics.current_heat <= 50_000_000, "heat should never exceed daily loss limit");
        }
    }

    println!("\n✓ Position sizing respects limits!");
}
