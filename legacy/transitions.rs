// RGB20 Library: high-level API to RGB fungible assets.
// Written in 2019-2022 by
//     Dr. Maxim Orlovsky <orlovsky@lnp-bp.org>
//
// To the extent possible under law, the author(s) have dedicated all copyright
// and related and neighboring rights to this software to the public domain
// worldwide. This software is distributed without any warranty.
//
// You should have received a copy of the MIT License along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

//! High-level RGB20 API performing asset issuance, transfers and other
//! asset-management operations

use std::collections::{BTreeMap, BTreeSet};

use bitcoin::OutPoint;
use chrono::Utc;
use lnpbp::chain::Chain;
use rgb::fungible::allocation::{
    Allocation, AllocationMap, AllocationValueMap, AllocationValueVec, IntoSealValueMap,
    OutpointValueMap, OutpointValueVec,
};
use rgb::prelude::*;
use rgb::secp256k1zkp;
use seals::txout::ExplicitSeal;
use stens::AsciiString;

use super::schema::{self, FieldType, OwnedRightType, TransitionType};
use super::{Asset, Issue};

/// Errors happening during construction of RGB-20 asset state transitions
#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Debug, Display, Error)]
#[display(doc_comments)]
pub enum Error {
    /// input {0} is not related to the contract
    UnrelatedInput(OutPoint),

    /// sum of inputs and outputs is not equal
    InputsNotEqualOutputs,

    /// issue allowance {allowed} for the provided set of issue-controlling
    /// rights is insufficient to issue the requested amount {requested}
    InsufficientIssueAllowance {
        /// Allowed issue value
        allowed: AtomicValue,
        /// Requested issue value
        requested: AtomicValue,
    },

    /// the requested supply {requested} does not match the total supply
    /// {assigned} allocated to the owned rights consumed by the operation
    SupplyMismatch {
        /// Assigned supply change rights
        assigned: AtomicValue,
        /// Requested supply change
        requested: AtomicValue,
    },

    /// method was provided with a set of seals for owned rights which are not
    /// a part of the asset data: {0:?}
    UnknownSeals(BTreeSet<OutPoint>),
}

impl Asset {
    /// Performs secondary issue closing an inflation-controlling seal over
    /// inflation state transition, which is constructed and returned by this
    /// function
    pub fn inflate(
        &self,
        closing: BTreeSet<OutPoint>,
        next_inflation: AllocationValueMap,
        allocations: AllocationValueVec,
    ) -> Result<Transition, Error> {
        let issued_supply = allocations.sum();
        let future_inflation: AtomicValue = next_inflation.sum();

        let input_issues: Vec<&Issue> = self
            .known_issues()
            .iter()
            .filter(|issue| {
                issue
                    .inflation_assignments()
                    .keys()
                    .find(|outpoint| closing.contains(outpoint))
                    .is_some()
            })
            .collect();

        let mut found_seals = bset![];
        let mut parent = ParentOwnedRights::default();
        let issue_allowance = input_issues.iter().fold(0u64, |sum, issue| {
            let issued: AtomicValue = issue
                .inflation_assignments()
                .iter()
                .filter(|(outpoint, _)| closing.contains(outpoint))
                .map(|(outpoint, (value, indexes))| {
                    indexes.into_iter().for_each(|index| {
                        parent
                            .entry(*issue.node_id())
                            .or_insert(empty!())
                            .entry(OwnedRightType::Inflation.into())
                            .or_insert(empty!())
                            .push(*index)
                    });
                    found_seals.insert(*outpoint);
                    value
                })
                .sum();
            sum + issued
        });

        if issue_allowance < issued_supply {
            return Err(Error::InsufficientIssueAllowance {
                allowed: issue_allowance,
                requested: issued_supply,
            });
        }

        if future_inflation + issued_supply != issue_allowance {
            return Err(Error::SupplyMismatch {
                assigned: issue_allowance,
                requested: issued_supply + future_inflation,
            });
        }

        if found_seals != closing {
            return Err(Error::UnknownSeals(closing));
        }

        let metadata = type_map! {
            FieldType::IssuedSupply => field!(U64, issued_supply)
        };

        let mut owned_rights = BTreeMap::new();
        owned_rights.insert(
            OwnedRightType::Assets.into(),
            AssignmentVec::zero_balanced(
                vec![value::Revealed {
                    value: issued_supply,
                    blinding: secp256k1zkp::key::ONE_KEY.into(),
                }],
                allocations.into_seal_value_map(),
                empty![],
            ),
        );
        if !next_inflation.is_empty() {
            owned_rights.insert(
                OwnedRightType::Inflation.into(),
                next_inflation.into_assignments(),
            );
        }

        let transition = Transition::with(
            TransitionType::Issue,
            metadata.into(),
            empty!(),
            owned_rights.into(),
            empty!(),
            parent,
        );

        Ok(transition)
    }

    /// Opens a new epoch by closing epoch-controlling seal over epoch opening
    /// state transition, which is constructed and returned by this function
    pub fn epoch(
        &self,
        closing: OutPoint,
        next_epoch: Option<ExplicitSeal>,
        burning_seal: Option<ExplicitSeal>,
    ) -> Result<Transition, Error> {
        todo!()
    }

    /// Burns certain amount of the asset by closing burn-controlling seal over
    /// proof-of-burn state transition, which is constructed and returned by
    /// this function
    pub fn burn(
        &self,
        closing: OutPoint,
        burned_value: AtomicValue,
        burned_utxos: BTreeSet<OutPoint>,
        next_burn: Option<ExplicitSeal>,
    ) -> Result<Transition, Error> {
        todo!()
    }

    /// Burns and re-allocates certain amount of the asset by closing
    /// burn-controlling seal over proof-of-burn state transition, which is
    /// constructed and returned by this function
    pub fn burn_replace(
        &self,
        closing: OutPoint,
        burned_value: AtomicValue,
        burned_utxos: BTreeSet<OutPoint>,
        next_burn: Option<ExplicitSeal>,
        allocations: AllocationValueVec,
    ) -> Result<Transition, Error> {
        todo!()
    }

    /// Function creates a fungible asset-specific state transition (i.e. RGB-20
    /// schema-based) given an asset information, inputs and desired outputs
    pub fn transfer(
        &self,
        inputs: BTreeSet<OutPoint>,
        payment: EndpointValueMap,
        change: SealValueMap,
    ) -> Result<Transition, Error> {
        // Collecting all input allocations
        let mut input_allocations = Vec::<Allocation>::new();
        for outpoint in inputs {
            let found = self.outpoint_allocations(outpoint);
            if found.len() == 0 {
                Err(Error::UnrelatedInput(outpoint))?
            }
            input_allocations.extend(found);
        }
        // Computing sum of inputs
        let total_inputs = input_allocations
            .iter()
            .fold(0u64, |acc, alloc| acc + alloc.revealed_amount().value);

        let total_outputs = change.sum() + payment.sum();

        if total_inputs != total_outputs {
            Err(Error::InputsNotEqualOutputs)?
        }

        let input_amounts = input_allocations
            .iter()
            .map(|alloc| *alloc.revealed_amount())
            .collect();
        let assignments = type_map! {
            OwnedRightType::Assets =>
            AssignmentVec::zero_balanced(input_amounts, change, payment)
        };

        let mut parent = ParentOwnedRights::default();
        for alloc in input_allocations {
            parent
                .entry(*alloc.node_id())
                .or_insert(empty!())
                .entry(OwnedRightType::Assets.into())
                .or_insert(empty!())
                .push(*alloc.index());
        }

        let transition = Transition::with(
            TransitionType::Transfer,
            empty!(),
            empty!(),
            assignments.into(),
            empty!(),
            parent,
        );

        Ok(transition)
    }
}
