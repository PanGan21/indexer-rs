// Copyright 2023-, Edge & Node, GraphOps, and Semiotic Labs.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use anyhow::anyhow;
use tap_core::{
    receipt::{
        checks::{Check, CheckError, CheckResult},
        state::Checking,
        ReceiptWithState,
    },
    signed_message::MessageId,
};

use crate::tap::context::error::AdapterError;

pub struct Value {
    query_appraisals: Option<Arc<RwLock<HashMap<MessageId, u128>>>>,
}

#[async_trait::async_trait]
impl Check for Value {
    async fn check(
        &self,
        _: &tap_core::receipt::Context,
        receipt: &ReceiptWithState<Checking>,
    ) -> CheckResult {
        let value = receipt.signed_receipt().message.value;
        let query_id = receipt.signed_receipt().unique_hash();

        let query_appraisals = self.query_appraisals.as_ref().expect(
            "Query appraisals should be initialized. The opposite should never happen when \
            receipts value checking is enabled.",
        );
        let query_appraisals_read = query_appraisals.read().unwrap();
        let appraised_value = query_appraisals_read
            .get(&query_id)
            .ok_or(AdapterError::ValidationError {
                error: "No appraised value found for query".to_string(),
            })
            .map_err(|e| CheckError::Failed(e.into()))?;
        if value != *appraised_value {
            return Err(CheckError::Failed(anyhow!(
                "Value different from appraised_value. value: {}, appraised_value: {}",
                value,
                *appraised_value
            )));
        }
        Ok(())
    }
}
