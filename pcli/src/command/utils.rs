use comfy_table::{presets, Table};
use penumbra_crypto::{
    asset::{self},
    dex::{lp::position::Position, DirectedUnitPair},
    fixpoint::U128x128,
};

pub(crate) fn render_positions(asset_cache: &asset::Cache, positions: &[Position]) -> String {
    let mut table = Table::new();
    table.load_preset(presets::NOTHING);
    table.set_header(vec![
        "ID",
        "Trading Pair",
        "State",
        "Reserves",
        "Trading Function",
    ]);

    for position in positions {
        let trading_pair = position.phi.pair;
        let denom_1 = asset_cache.get(&trading_pair.asset_1());
        let denom_2 = asset_cache.get(&trading_pair.asset_2());

        // if either of the assets is unknown, just show the asset IDs
        let (d_display, v_display) = if denom_1.is_none() || denom_2.is_none() {
            (
                format!("({}, {})", trading_pair.asset_1(), trading_pair.asset_2()),
                format!(
                    "({} {}, {} {})",
                    position.reserves.r1,
                    trading_pair.asset_1(),
                    position.reserves.r2,
                    trading_pair.asset_2(),
                ),
            )
        } else {
            let display_denom_1 = denom_1.unwrap().default_unit();
            let display_denom_2 = denom_2.unwrap().default_unit();
            (
                format!("({}, {})", display_denom_1, display_denom_2),
                format!(
                    "({}, {})",
                    display_denom_1.format_value(position.reserves.r1),
                    display_denom_2.format_value(position.reserves.r2)
                ),
            )
        };

        table.add_row(vec![
            format!("{}", position.id()),
            d_display,
            position.state.to_string(),
            v_display,
            format!(
                "fee: {}bps, p/q: {:.6}, q/p: {:.18}",
                position.phi.component.fee,
                f64::from(position.phi.component.p) / f64::from(position.phi.component.q),
                f64::from(position.phi.component.q) / f64::from(position.phi.component.p),
            ),
        ]);
    }

    format!("{table}")
}

pub(crate) fn render_xyk_approximation(pair: DirectedUnitPair, positions: &[Position]) -> String {
    let mut table = Table::new();
    table.load_preset(presets::NOTHING);
    table.set_header(vec![
        "ID",
        "Trading Pair",
        "Reserves",
        "Pool fee",
        "Price summary",
        format!("{} to {}", pair.start, pair.end).as_str(),
        format!("{} to {}", pair.end, pair.start).as_str(),
    ]);

    for position in positions {
        let fee = position.phi.component.fee;
        let well_directed_phi = position.phi.orient_end(pair.end.id()).unwrap();
        // TODO(erwan): refactor with `BuyOrder`?
        let start = pair.start.unit_amount();
        let end = pair.end.unit_amount();
        let fp_start: U128x128 = start.try_into().unwrap();
        let fp_end: U128x128 = end.try_into().unwrap();
        let cross = (fp_start / fp_end).unwrap();
        let cross_inv = (fp_end / fp_start).unwrap();
        let price_a_to_b = (well_directed_phi.effective_price_inv() * cross).unwrap();
        let price_b_to_a = (well_directed_phi.effective_price() * cross_inv).unwrap();

        let asset_1_to_2 = format!("{} @ {:>10.05}", pair.to_string(), price_a_to_b);
        let asset_2_to_1 = format!("{} @ {:>10.05}", pair.flip().to_string(), price_b_to_a);

        // We display a price summary that is the relevant cost of conversion (since one of the reserves is 0).
        let price_summary = if position.reserves_for(pair.end.id()).unwrap() == 0u64.into() {
            asset_2_to_1.clone()
        } else {
            asset_1_to_2.clone()
        };

        table.add_row(vec![
            format!("{}", position.id()),
            format!("{}", pair.to_string()),
            format!(
                "({}, {})",
                pair.start
                    .format_value(position.reserves_for(pair.start.id()).unwrap()),
                pair.end
                    .format_value(position.reserves_for(pair.end.id()).unwrap()),
            ),
            format!("{fee:>5}bps"),
            price_summary,
            asset_1_to_2,
            asset_2_to_1,
        ]);
    }

    format!("{table}")
}
