use heed::types::*;
use heed::{Database, RoTxn, RwTxn};
use hivemind_types::{sdk_authorization_ed25519_dalek, sdk_types, sdk_types::OutPoint, *};
use nalgebra::{DVector, Vector2};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use sdk_types::GetValue as _;
use std::collections::HashMap;

struct State {
    pub utxos: Database<SerdeBincode<OutPoint>, SerdeBincode<Output>>,
    pub markets: Database<SerdeBincode<OutPoint>, SerdeBincode<Vec<u64>>>,
}

impl State {
    pub fn new(env: &heed::Env) -> Result<Self, Error> {
        let utxos = env.create_database(Some("utxos"))?;
        let markets = env.create_database(Some("markets"))?;
        Ok(State { utxos, markets })
    }

    pub fn fill_transaction(
        &self,
        txn: &RoTxn,
        transaction: &Transaction,
    ) -> Result<FilledTransaction, Error> {
        let mut inputs = vec![];
        for input in &transaction.inputs {
            let utxo = self
                .utxos
                .get(txn, input)?
                .ok_or(Error::NoUtxo { outpoint: *input })?;
        }
        Ok(FilledTransaction {
            inputs,
            outputs: transaction.outputs.clone(),
        })
    }

    fn get_deltas_and_values(
        &self,
        txn: &RoTxn,
        transaction: &FilledTransaction,
    ) -> Result<(HashMap<OutPoint, DVector<Decimal>>, u64, u64), Error> {
        // TODO: Use more efficient hash maps (there is no need to hash OutPoints).
        let mut markets: HashMap<OutPoint, (u64, u32)> = HashMap::new();
        let mut market_to_delta: HashMap<OutPoint, DVector<Decimal>> = HashMap::new();
        let mut input_value: u64 = 0;
        for input in &transaction.inputs {
            input_value += input.get_value();
            match input.content {
                sdk_types::Content::Custom(HivemindContent::Position {
                    market,
                    share,
                    value,
                }) => {
                    let (_, size) = markets.entry(market).or_insert({
                        let market = self
                            .utxos
                            .get(txn, &market)?
                            .ok_or(Error::NoUtxo { outpoint: market })?;
                        match market.content {
                            sdk_types::Content::Custom(HivemindContent::Market { b, size }) => {
                                (b, size)
                            }
                            _ => unreachable!(),
                        }
                    });
                    let delta = market_to_delta
                        .entry(market)
                        .or_insert(DVector::from_element(*size as usize, dec!(0)));
                    delta[share as usize] -= Decimal::from(value);
                }
                _ => {}
            };
        }
        let mut output_value: u64 = 0;
        for output in &transaction.outputs {
            output_value += output.get_value();
            match output.content {
                sdk_types::Content::Custom(HivemindContent::Position {
                    market,
                    share,
                    value,
                }) => {
                    let (_, size) = markets.entry(market).or_insert({
                        let market = self
                            .utxos
                            .get(txn, &market)?
                            .ok_or(Error::NoUtxo { outpoint: market })?;
                        match market.content {
                            sdk_types::Content::Custom(HivemindContent::Market { b, size }) => {
                                (b, size)
                            }
                            _ => unreachable!(),
                        }
                    });
                    let delta = market_to_delta
                        .entry(market)
                        .or_insert(DVector::from_element(*size as usize, dec!(0)));
                    delta[share as usize] += Decimal::from(value);
                }
                _ => {}
            };
        }
        Ok((market_to_delta, input_value, output_value))
    }

    fn get_cost(
        &self,
        txn: &RoTxn,
        market_to_delta: &HashMap<OutPoint, DVector<Decimal>>,
    ) -> Result<u64, Error> {
        let mut total_cost: u64 = 0;
        for (market, delta) in market_to_delta {
            let state: Vec<Decimal> = self
                .markets
                .get(txn, market)?
                .ok_or(Error::NoUtxo { outpoint: *market })?
                .iter()
                .map(|n| Decimal::from(*n))
                .collect();
            let state = DVector::from(state);
            let (b, size) = {
                let market = self
                    .utxos
                    .get(txn, &market)?
                    .ok_or(Error::NoUtxo { outpoint: *market })?;
                match market.content {
                    sdk_types::Content::Custom(HivemindContent::Market { b, size }) => (b, size),
                    _ => unreachable!(),
                }
            };
            let cost = lmsr_cost(Decimal::from(b), &(state.clone() + delta))
                - lmsr_cost(Decimal::from(b), &state);
            total_cost += cost.to_u64().ok_or(Error::U64Overflow { decimal: cost })?;
        }
        Ok(total_cost)
    }

    pub fn validate_transaction(
        &self,
        txn: &RoTxn,
        transaction: &FilledTransaction,
    ) -> Result<(), Error> {
        let (market_to_delta, input_value, output_value) =
            self.get_deltas_and_values(txn, transaction)?;
        let cost = self.get_cost(txn, &market_to_delta)?;
        if cost + output_value > input_value {
            return Err(Error::NotEnoughValueIn {
                cost,
                output_value,
                input_value,
            });
        }
        Ok(())
    }
}

fn lmsr_cost(b: Decimal, state: &DVector<Decimal>) -> Decimal {
    let max_money = dec!(21_000_000_00_000_000);
    state.map(|q| (q / (b * max_money)).exp()).sum().ln() * b * max_money
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
    #[error("number {decimal} doesn't fit in a u64")]
    U64Overflow { decimal: Decimal },
    #[error("value in is not enough to cover amm trade cost and value out: {cost} + {output_value} > {input_value}")]
    NotEnoughValueIn {
        cost: u64,
        input_value: u64,
        output_value: u64,
    },
}
