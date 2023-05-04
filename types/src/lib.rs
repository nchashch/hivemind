pub use sdk_authorization_ed25519_dalek;
use sdk_authorization_ed25519_dalek::*;
pub use sdk_types;
use sdk_types::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HivemindContent {
    // VoteCoin {
    //     value: u64,
    // },
    // Decision {
    //     query: sdk_types::Hash,
    //     size: u32,
    //     deadline_height: u32,
    // },
    // Resolution {
    //     decision: sdk_types::OutPoint,
    //     outcome: Option<u32>,
    // },
    Market {
        b: u64,
        size: u32,
    },
    // IDEA: Don't require fees when people spend Share outputs
    // to incentivize them to keep the UTXO set small.
    //
    // TODO: Maybe use HashMap<ShareId, Value> instead
    // of separate outputs for every share_id. It would reduce
    Position {
        market: sdk_types::OutPoint,
        share: u32,
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
    pub inputs: Vec<Output>,
    pub outputs: Vec<Output>,
}

pub type Output = sdk_types::Output<HivemindContent>;
pub type Transaction = sdk_types::Transaction<HivemindContent>;
pub type AuthorizedTransaction = sdk_types::AuthorizedTransaction<Authorization, HivemindContent>;
pub type Body = sdk_types::Body<Authorization, HivemindContent>;
