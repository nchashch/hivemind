use hivemind_types::{
    nalgebra, rust_decimal, rust_decimal_macros, sdk_authorization_ed25519_dalek, sdk_types,
};

use nalgebra::DVector;
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use sdk_types::GetValue as _;

use heed::{types::*, RwTxn};
use heed::{Database, RoTxn};
use hivemind_types::{sdk_types::OutPoint, *};
use std::collections::{HashMap, HashSet};

pub struct State {
    pub utxos: Database<SerdeBincode<OutPoint>, SerdeBincode<Output>>,
    pub vectors: Database<SerdeBincode<OutPoint>, SerdeBincode<Vec<Decimal>>>,
    pub markets: Database<SerdeBincode<OutPoint>, SerdeBincode<Market>>,
    // There is some aparent redundancy, position outpoints are stored twice: once as keys in utxos
    // db and once as values in market_to_positions db.
    pub market_to_positions: Database<SerdeBincode<OutPoint>, SerdeBincode<Vec<OutPoint>>>,
}

impl State {
    pub const NUM_DBS: u32 = 4;

    pub fn new(env: &heed::Env) -> Result<Self, Error> {
        let utxos = env.create_database(Some("utxos"))?;
        let vectors = env.create_database(Some("vectors"))?;
        let markets = env.create_database(Some("markets"))?;
        let market_to_positions = env.create_database(Some("market_to_positions"))?;
        Ok(State {
            utxos,
            vectors,
            markets,
            market_to_positions,
        })
    }

    pub fn fill_transaction(
        &self,
        txn: &RoTxn,
        transaction: &Transaction,
    ) -> Result<FilledTransaction, Error> {
        let mut spent_utxos = vec![];
        for input in &transaction.inputs {
            let utxo = self
                .utxos
                .get(txn, input)?
                .ok_or(Error::NoUtxo { outpoint: *input })?;
            spent_utxos.push(utxo);
        }
        Ok(FilledTransaction {
            spent_utxos,
            transaction: transaction.clone(),
        })
    }

    fn get_deltas_and_values(
        &self,
        txn: &RoTxn,
        transaction: &FilledTransaction,
    ) -> Result<(HashMap<OutPoint, DVector<Decimal>>, u64, u64), Error> {
        // TODO: Use more efficient hash maps (there is no need to hash
        // OutPoints).
        let mut market_to_delta: HashMap<OutPoint, DVector<Decimal>> = HashMap::new();
        let mut input_value: u64 = 0;
        for spent_utxo in &transaction.spent_utxos {
            input_value += spent_utxo.get_value();
            match &spent_utxo.content {
                sdk_types::Content::Custom(HivemindContent::Position {
                    market,
                    share,
                    value,
                }) => {
                    let size = self.get_size(txn, &market)?;
                    let delta = market_to_delta
                        .entry(*market)
                        .or_insert(DVector::from_element(size as usize, dec!(0)));
                    let flat_index = self.share_to_flat_index(txn, &market, &share)?;
                    delta[flat_index as usize] -= Decimal::from(*value);
                }
                _ => {}
            };
        }
        let mut output_value: u64 = 0;
        for output in &transaction.transaction.outputs {
            output_value += output.get_value();
            // It costs `b * ln(size)` to create a new market with `size` possible outcomes.
            //
            // This is not covered by get_value() because once created spent Market UTXOs don't
            // count towards input_value.
            //
            // But when a market is resolved, its value would = to the market authors share in
            // fees.
            output_value += self.get_market_funding_cost(txn, output)?;
            match &output.content {
                sdk_types::Content::Custom(HivemindContent::Position {
                    market,
                    share,
                    value,
                }) => {
                    let size = self.get_size(txn, &market)?;
                    let delta = market_to_delta
                        .entry(*market)
                        .or_insert(DVector::from_element(size as usize, dec!(0)));
                    let flat_index = self.share_to_flat_index(txn, &market, &share)?;
                    delta[flat_index as usize] += Decimal::from(*value);
                }
                _ => {}
            };
        }
        Ok((market_to_delta, input_value, output_value))
    }

    fn share_to_flat_index(
        &self,
        txn: &RoTxn,
        market: &OutPoint,
        share: &[u32],
    ) -> Result<u32, Error> {
        let market = self
            .markets
            .get(txn, market)?
            .ok_or(Error::NoUtxo { outpoint: *market })?;

        let mut step: u32 = market.shape.iter().product();
        let mut flat_index = 0;
        for (index, size) in share.iter().zip(market.shape.iter()) {
            step /= size;
            flat_index += index * step;
        }
        Ok(flat_index)
    }

    fn get_size(&self, txn: &RoTxn, market: &OutPoint) -> Result<u32, Error> {
        let market = self
            .markets
            .get(txn, &market)?
            .ok_or(Error::NoUtxo { outpoint: *market })?;
        Ok(market.shape.iter().product())
    }

    fn get_cost(
        &self,
        txn: &RoTxn,
        market_to_delta: &HashMap<OutPoint, DVector<Decimal>>,
    ) -> Result<Decimal, Error> {
        let mut total_cost: Decimal = dec!(0);
        for (market, delta) in market_to_delta {
            let state: Vec<Decimal> = self
                .vectors
                .get(txn, market)?
                .ok_or(Error::NoUtxo { outpoint: *market })?
                .iter()
                .copied()
                .collect();
            let state = DVector::from(state);
            let b = {
                let market = self
                    .utxos
                    .get(txn, market)?
                    .ok_or(Error::NoUtxo { outpoint: *market })?;
                match market.content {
                    sdk_types::Content::Custom(HivemindContent::Market { b, .. }) => b,
                    _ => unreachable!(),
                }
            };
            let cost = lmsr_cost(Decimal::from(b), &(state.clone() + delta))
                - lmsr_cost(Decimal::from(b), &state);
            total_cost += cost;
        }
        Ok(total_cost)
    }

    // TODO: Check that input_value in is enough to cover market creation.
    pub fn validate_transaction(
        &self,
        txn: &RoTxn,
        transaction: &FilledTransaction,
        height: u32,
    ) -> Result<u64, Error> {
        let mut resolved_decisions = HashSet::new();
        let mut spent_decisions = vec![];
        for (outpoint, spent_utxo) in transaction
            .transaction
            .inputs
            .iter()
            .zip(transaction.spent_utxos.iter())
        {
            match &spent_utxo.content {
                sdk_types::Content::Custom(HivemindContent::Decision {
                    resolvable_height, ..
                }) => {
                    if *resolvable_height < height {
                        return Err(Error::DecisionSpentEarly);
                    }
                    spent_decisions.push(outpoint);
                }
                sdk_types::Content::Custom(HivemindContent::Resolution { decision, .. }) => {
                    resolved_decisions.insert(decision);
                }
                sdk_types::Content::Custom(HivemindContent::Market { decisions, .. }) => {
                    for decision in decisions {
                        let decision = self.utxos.get(txn, decision)?.ok_or(Error::NoUtxo {
                            outpoint: *decision,
                        })?;
                        match decision.content {
                            sdk_types::Content::Custom(HivemindContent::Decision {
                                resolvable_height,
                                ..
                            }) => {
                                if height > resolvable_height {
                                    return Err(Error::MarketUsingResolvableDecision);
                                }
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                _ => {}
            }
        }
        for spent_decision in &spent_decisions {
            if !resolved_decisions.contains(spent_decision) {
                return Err(Error::DecisionSpentWithoutResolution);
            }
        }
        let (market_to_delta, input_value, output_value) =
            self.get_deltas_and_values(txn, transaction)?;
        let cost = self.get_cost(txn, &market_to_delta)?;
        // NOTE: Cost is *negative* when you are selling shares.
        if cost + Decimal::from(output_value) > Decimal::from(input_value) {
            return Err(Error::NotEnoughValueIn);
        }
        let fee =
            input_value - cost.to_u64().ok_or(Error::U64Overflow { decimal: cost })? + output_value;
        Ok(fee)
    }

    pub fn validate_body(&self, txn: &RoTxn, body: Body) -> Result<(), Error> {
        let mut fee_value = 0;
        {
            let mut spent = HashSet::new();
            for transaction in &body.transactions {
                for input in &transaction.inputs {
                    if spent.contains(input) {
                        return Err(Error::UtxoDoubleSpent { outpoint: *input });
                    }
                    spent.insert(input);
                    let transaction = self.fill_transaction(txn, transaction)?;
                    fee_value += self.validate_transaction(txn, &transaction, 0)?;
                }
            }
        }
        let mut coinbase_value = 0;
        for output in &body.coinbase {
            coinbase_value += output.get_value();
        }

        if coinbase_value > fee_value {
            return Err(Error::NotEnoughFeeValue);
        }
        Ok(())
    }

    pub fn connect_body(&self, txn: &mut RwTxn, body: &Body) -> Result<(), Error> {
        let mut body_market_to_delta = HashMap::new();
        let mut decision_to_outcome = HashMap::new();
        for transaction in &body.transactions {
            for input in &transaction.inputs {
                self.utxos.delete(txn, input)?;
                panic!("this is incorrect! Delete data from other dbs as well.");
            }
            let txid = transaction.txid();
            for (vout, output) in transaction.outputs.iter().enumerate() {
                let outpoint = OutPoint::Regular {
                    txid,
                    vout: vout as u32,
                };
                self.utxos.put(txn, &outpoint, output)?;

                match &output.content {
                    sdk_types::Content::Custom(HivemindContent::Position { market, .. }) => {
                        let mut positions = self
                            .market_to_positions
                            .get(txn, market)?
                            .ok_or(Error::NoUtxo { outpoint: *market })?;
                        positions.push(outpoint);
                        self.market_to_positions.put(txn, market, &positions)?;
                    }
                    sdk_types::Content::Custom(HivemindContent::Resolution {
                        decision,
                        outcome,
                    }) => {
                        decision_to_outcome.insert(decision, *outcome);
                    }
                    sdk_types::Content::Custom(HivemindContent::Market { b, decisions }) => {
                        let mut shape = vec![];
                        for decision in decisions {
                            let decision = self.utxos.get(txn, decision)?.ok_or(Error::NoUtxo {
                                outpoint: *decision,
                            })?;
                            let size = match decision.content {
                                sdk_types::Content::Custom(HivemindContent::Decision {
                                    size,
                                    ..
                                }) => size,
                                _ => unreachable!(),
                            };
                            shape.push(size);
                        }
                        let outcomes = std::iter::repeat(None).take(shape.len()).collect();

                        self.markets.put(
                            txn,
                            &outpoint,
                            &Market {
                                b: *b,
                                decisions: decisions.clone(),
                                shape,
                                outcomes,
                            },
                        )?;
                    }
                    _ => {}
                }
            }
            let transaction = self.fill_transaction(txn, transaction)?;
            let (market_to_delta, _, _) = self.get_deltas_and_values(txn, &transaction)?;
            for (market, delta) in &market_to_delta {
                let body_delta = body_market_to_delta
                    .entry(*market)
                    .or_insert(DVector::from_element(delta.len(), dec!(0)));
                *body_delta += delta;
            }
        }
        for (market, delta) in &body_market_to_delta {
            let state = self
                .vectors
                .get(txn, market)?
                .ok_or(Error::NoUtxo { outpoint: *market })?;
            let state = DVector::from(state);
            let new_state = state + delta;
            let new_state: Vec<Decimal> = new_state.iter().copied().collect();
            self.vectors.put(txn, market, &new_state)?;
        }

        // After all market decisions are resolved the market itself is resolved.
        // All Position outputs with share == outcome turn into Value outputs.
        // All other Position outputs are removed.

        let mut updated_markets = vec![];
        let mut resolved_markets = vec![];
        for item in self.markets.iter(txn)? {
            let (outpoint, market) = item?;
            let mut outcomes = vec![];
            for decision in &market.decisions {
                outcomes.push(decision_to_outcome.get(decision).copied());
            }
            if outcomes.iter().all(Option::is_some) {
                let outcomes: Vec<u32> = outcomes.iter().copied().map(Option::unwrap).collect();
                resolved_markets.push((outpoint, outcomes));
            }
            updated_markets.push((outpoint, Market { outcomes, ..market }));
        }
        for (outpoint, outcomes) in &resolved_markets {
            let resolved_positions =
                self.market_to_positions
                    .get(txn, &outpoint)?
                    .ok_or(Error::NoUtxo {
                        outpoint: *outpoint,
                    })?;
            for position_outpoint in &resolved_positions {
                let position = self
                    .utxos
                    .get(txn, position_outpoint)?
                    .ok_or(Error::NoUtxo {
                        outpoint: *outpoint,
                    })?;
                match &position.content {
                    sdk_types::Content::Custom(HivemindContent::Position {
                        share, value, ..
                    }) => {
                        if share == outcomes {
                            let content = sdk_types::Content::<HivemindContent>::Value(*value);
                            self.utxos.put(
                                txn,
                                position_outpoint,
                                &Output {
                                    content,
                                    ..position.clone()
                                },
                            )?;
                        } else {
                            self.utxos.delete(txn, position_outpoint)?;
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
        for (outpoint, market) in &updated_markets {
            self.markets.put(txn, outpoint, market)?;
        }
        Ok(())
    }

    fn get_market_funding_cost(&self, txn: &RoTxn, output: &Output) -> Result<u64, Error> {
        match &output.content {
            sdk_types::Content::Custom(HivemindContent::Market { b, decisions }) => {
                let mut size: u32 = 1;
                for outpoint in decisions {
                    let decision = self.utxos.get(txn, outpoint)?.ok_or(Error::NoUtxo {
                        outpoint: *outpoint,
                    })?;
                    size *= match decision.content {
                        sdk_types::Content::Custom(HivemindContent::Decision { size, .. }) => {
                            Ok(size)
                        }
                        _ => Err(Error::InvalidOutPoint {
                            outpoint: *outpoint,
                        }),
                    }?;
                }
                let b = Decimal::from(*b);
                let size = Decimal::from(size);
                let cost = b * size.ln();
                cost.to_u64().ok_or(Error::U64Overflow { decimal: cost })
            }
            _ => Ok(0),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("authorization error")]
    Authorization(#[from] sdk_authorization_ed25519_dalek::Error),
    #[error("sdk error")]
    Sdk(#[from] sdk_types::Error),
    #[error("heed error")]
    Heed(#[from] heed::Error),
    #[error("utxo {outpoint} doesn't exist")]
    NoUtxo { outpoint: OutPoint },
    #[error("outpoint {outpoint} doesn't refer to a valid market")]
    InvalidOutPoint { outpoint: OutPoint },
    #[error("number {decimal} doesn't fit in a u64")]
    U64Overflow { decimal: Decimal },
    #[error("value in is not enough to cover amm trade cost and value out")]
    NotEnoughValueIn,
    #[error("fee value is not enough to cover coinbase value out")]
    NotEnoughFeeValue,
    #[error("utxo {outpoint} was spent more than once in this block")]
    UtxoDoubleSpent { outpoint: OutPoint },
    #[error("decision output is spent before its resolvable height was reached")]
    DecisionSpentEarly,
    #[error("decision output is spent without a resolution output being created")]
    DecisionSpentWithoutResolution,
    #[error("can't create market using a decision that is already resolvable at this height")]
    MarketUsingResolvableDecision,
}
