use nalgebra::DVector;
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use sdk_authorization_ed25519_dalek::*;
use sdk_types::*;
use serde::{Deserialize, Serialize};

pub use nalgebra;
pub use rust_decimal;
pub use rust_decimal_macros;
pub use sdk_authorization_ed25519_dalek;
pub use sdk_types;

// Maybe accounts model would work better for this?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HivemindContent {
    // VoteCoin {
    //     value: u64,
    // },
    Resolution {
        decision: OutPoint,
        outcome: u32,
    },
    Decision {
        query: Hash,
        size: u32,
        resolvable_height: u32,
    },
    Market {
        // (x0 || x1 || x2) && (x3 || x4)
        b: u64,
        decisions: Vec<OutPoint>,
    },
    // IDEA: Don't require fees when people spend Share outputs
    // to incentivize them to keep the UTXO set small.
    //
    // TODO: Maybe use HashMap<ShareId, Value> instead of separate outputs for every share_id. It
    // would reduce the number of UTXOs.
    Position {
        market: sdk_types::OutPoint,
        share: Vec<u32>,
        value: u64,
    },
}

impl GetValue for HivemindContent {
    #[inline(always)]
    fn get_value(&self) -> u64 {
        0
    }
}

pub struct FilledTransaction {
    pub spent_utxos: Vec<Output>,
    pub transaction: Transaction,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Market {
    pub b: u64,
    pub shape: Vec<u32>,
    pub decisions: Vec<OutPoint>,
    pub outcomes: Vec<Option<u32>>,
}

pub type Output = sdk_types::Output<HivemindContent>;
pub type Transaction = sdk_types::Transaction<HivemindContent>;
pub type AuthorizedTransaction = sdk_types::AuthorizedTransaction<Authorization, HivemindContent>;
pub type Body = sdk_types::Body<Authorization, HivemindContent>;

pub fn lmsr_cost(b: Decimal, state: &DVector<Decimal>) -> Decimal {
    // We multiply b by max_money to avoid exp overflow.
    let max_money = dec!(21_000_000_00_000_000);
    state.map(|q| (q / (b * max_money)).exp()).sum().ln() * b * max_money
}
