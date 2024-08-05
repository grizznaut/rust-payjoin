//! Receive Payjoin
//!
//! This module contains types and methods used to receive payjoin via BIP78.
//! Usage is pretty simple:
//!
//! 1. Generate a pj_uri [BIP 21](https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki)
//!    using [`payjoin::Uri`](crate::Uri)::from_str
//! 2. Listen for a sender's request on the `pj` endpoint
//! 3. Parse the request using
//!    [`UncheckedProposal::from_request()`](crate::receive::UncheckedProposal::from_request())
//! 4. Validate the proposal using the `check` methods to guide you.
//! 5. Assuming the proposal is valid, augment it into a payjoin with the available
//!    `try_preserving_privacy` and `contribute` methods
//! 6. Extract the payjoin PSBT and sign it
//! 7. Respond to the sender's http request with the signed PSBT as payload.
//!
//! The `receive` feature provides all of the check methods, PSBT data manipulation, coin
//! selection, and transport structures to receive payjoin and handle errors in a privacy
//! preserving way.
//!
//! Receiving payjoin entails listening to a secure http endpoint for inbound requests.  The
//! endpoint is displayed in the `pj` parameter of a [bip
//! 21](https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki) request URI.
//!
//! [reference implementation](https://github.com/payjoin/rust-payjoin/tree/master/payjoin-cli)

use std::cmp::{max, min};
use std::collections::HashMap;

use bitcoin::base64::prelude::BASE64_STANDARD;
use bitcoin::base64::Engine;
use bitcoin::psbt::Psbt;
use bitcoin::{Amount, FeeRate, OutPoint, Script, TxOut};

mod error;
mod optional_parameters;
#[cfg(feature = "v2")]
pub mod v2;

use bitcoin::secp256k1::rand::seq::SliceRandom;
use bitcoin::secp256k1::rand::{self, Rng};
pub use error::{Error, RequestError, SelectionError};
use error::{InternalRequestError, InternalSelectionError};
use optional_parameters::Params;

use crate::input_type::InputType;
use crate::psbt::PsbtExt;

pub trait Headers {
    fn get_header(&self, key: &str) -> Option<&str>;
}

/// The sender's original PSBT and optional parameters
///
/// This type is used to process the request. It is returned by
/// [`UncheckedProposal::from_request()`](crate::receive::UncheckedProposal::from_request()).
///
/// If you are implementing an interactive payment processor, you should get extract the original
/// transaction with extract_tx_to_schedule_broadcast() and schedule, followed by checking
/// that the transaction can be broadcast with check_broadcast_suitability. Otherwise it is safe to
/// call assume_interactive_receive to proceed with validation.
#[derive(Debug, Clone)]
pub struct UncheckedProposal {
    psbt: Psbt,
    params: Params,
}

impl UncheckedProposal {
    pub fn from_request(
        mut body: impl std::io::Read,
        query: &str,
        headers: impl Headers,
    ) -> Result<Self, RequestError> {
        let content_type = headers
            .get_header("content-type")
            .ok_or(InternalRequestError::MissingHeader("Content-Type"))?;
        if !content_type.starts_with("text/plain") {
            return Err(InternalRequestError::InvalidContentType(content_type.to_owned()).into());
        }
        let content_length = headers
            .get_header("content-length")
            .ok_or(InternalRequestError::MissingHeader("Content-Length"))?
            .parse::<u64>()
            .map_err(InternalRequestError::InvalidContentLength)?;
        // 4M block size limit with base64 encoding overhead => maximum reasonable size of content-length
        if content_length > 4_000_000 * 4 / 3 {
            return Err(InternalRequestError::ContentLengthTooLarge(content_length).into());
        }

        // enforce the limit
        let mut buf = vec![0; content_length as usize]; // 4_000_000 * 4 / 3 fits in u32
        body.read_exact(&mut buf).map_err(InternalRequestError::Io)?;
        let base64 = BASE64_STANDARD.decode(&buf).map_err(InternalRequestError::Base64)?;
        let unchecked_psbt = Psbt::deserialize(&base64).map_err(InternalRequestError::Psbt)?;

        let psbt = unchecked_psbt.validate().map_err(InternalRequestError::InconsistentPsbt)?;
        log::debug!("Received original psbt: {:?}", psbt);

        let pairs = url::form_urlencoded::parse(query.as_bytes());
        let params = Params::from_query_pairs(pairs).map_err(InternalRequestError::SenderParams)?;
        log::debug!("Received request with params: {:?}", params);

        // TODO check that params are valid for the request's Original PSBT

        Ok(UncheckedProposal { psbt, params })
    }

    /// The Sender's Original PSBT transaction
    pub fn extract_tx_to_schedule_broadcast(&self) -> bitcoin::Transaction {
        self.psbt.clone().extract_tx_unchecked_fee_rate()
    }

    fn psbt_fee_rate(&self) -> Result<FeeRate, Error> {
        let original_psbt_fee = self.psbt.fee().map_err(InternalRequestError::Psbt)?;
        Ok(original_psbt_fee / self.extract_tx_to_schedule_broadcast().weight())
    }

    /// Check that the Original PSBT can be broadcasted.
    ///
    /// Receiver MUST check that the Original PSBT from the sender
    /// can be broadcast, i.e. `testmempoolaccept` bitcoind rpc returns { "allowed": true,.. }.
    ///
    /// Receiver can optionaly set a minimum feerate that will be enforced on the Original PSBT.
    /// This can be used to prevent probing attacks and make it easier to deal with
    /// high feerate environments.
    ///
    /// Do this check if you generate bitcoin uri to receive Payjoin on sender request without manual human approval, like a payment processor.
    /// Such so called "non-interactive" receivers are otherwise vulnerable to probing attacks.
    /// If a sender can make requests at will, they can learn which bitcoin the receiver owns at no cost.
    /// Broadcasting the Original PSBT after some time in the failure case makes incurs sender cost and prevents probing.
    ///
    /// Call this after checking downstream.
    pub fn check_broadcast_suitability(
        self,
        min_fee_rate: Option<FeeRate>,
        can_broadcast: impl Fn(&bitcoin::Transaction) -> Result<bool, Error>,
    ) -> Result<MaybeInputsOwned, Error> {
        let original_psbt_fee_rate = self.psbt_fee_rate()?;
        if let Some(min_fee_rate) = min_fee_rate {
            if original_psbt_fee_rate < min_fee_rate {
                return Err(InternalRequestError::PsbtBelowFeeRate(
                    original_psbt_fee_rate,
                    min_fee_rate,
                )
                .into());
            }
        }
        if can_broadcast(&self.psbt.clone().extract_tx_unchecked_fee_rate())? {
            Ok(MaybeInputsOwned { psbt: self.psbt, params: self.params })
        } else {
            Err(InternalRequestError::OriginalPsbtNotBroadcastable.into())
        }
    }

    /// Call this method if the only way to initiate a Payjoin with this receiver
    /// requires manual intervention, as in most consumer wallets.
    ///
    /// So-called "non-interactive" receivers, like payment processors, that allow arbitrary requests are otherwise vulnerable to probing attacks.
    /// Those receivers call `extract_tx_to_check_broadcast()` and `attest_tested_and_scheduled_broadcast()` after making those checks downstream.
    pub fn assume_interactive_receiver(self) -> MaybeInputsOwned {
        MaybeInputsOwned { psbt: self.psbt, params: self.params }
    }
}

/// Typestate to validate that the Original PSBT has no receiver-owned inputs.
///
/// Call [`check_no_receiver_owned_inputs()`](struct.UncheckedProposal.html#method.check_no_receiver_owned_inputs) to proceed.
#[derive(Clone)]
pub struct MaybeInputsOwned {
    psbt: Psbt,
    params: Params,
}

impl MaybeInputsOwned {
    /// Check that the Original PSBT has no receiver-owned inputs.
    /// Return original-psbt-rejected error or otherwise refuse to sign undesirable inputs.
    ///
    /// An attacker could try to spend receiver's own inputs. This check prevents that.
    pub fn check_inputs_not_owned(
        self,
        is_owned: impl Fn(&Script) -> Result<bool, Error>,
    ) -> Result<MaybeMixedInputScripts, Error> {
        let mut err = Ok(());
        if let Some(e) = self
            .psbt
            .input_pairs()
            .scan(&mut err, |err, input| match input.previous_txout() {
                Ok(txout) => Some(txout.script_pubkey.to_owned()),
                Err(e) => {
                    **err = Err(Error::BadRequest(InternalRequestError::PrevTxOut(e).into()));
                    None
                }
            })
            .find_map(|script| match is_owned(&script) {
                Ok(false) => None,
                Ok(true) =>
                    Some(Error::BadRequest(InternalRequestError::InputOwned(script).into())),
                Err(e) => Some(Error::Server(e.into())),
            })
        {
            return Err(e);
        }
        err?;

        Ok(MaybeMixedInputScripts { psbt: self.psbt, params: self.params })
    }
}

/// Typestate to validate that the Original PSBT has no mixed input types.
///
/// Call [`check_no_mixed_input_types`](struct.UncheckedProposal.html#method.check_no_mixed_input_scripts) to proceed.
#[derive(Clone)]
pub struct MaybeMixedInputScripts {
    psbt: Psbt,
    params: Params,
}

impl MaybeMixedInputScripts {
    /// Verify the original transaction did not have mixed input types
    /// Call this after checking downstream.
    ///
    /// Note: mixed spends do not necessarily indicate distinct wallet fingerprints.
    /// This check is intended to prevent some types of wallet fingerprinting.
    pub fn check_no_mixed_input_scripts(self) -> Result<MaybeInputsSeen, RequestError> {
        let mut err = Ok(());
        let input_scripts = self
            .psbt
            .input_pairs()
            .scan(&mut err, |err, input| match input.previous_txout() {
                Ok(txout) => match InputType::from_spent_input(txout, input.psbtin) {
                    Ok(input_script) => Some(input_script),
                    Err(e) => {
                        **err = Err(RequestError::from(InternalRequestError::InputType(e)));
                        None
                    }
                },
                Err(e) => {
                    **err = Err(RequestError::from(InternalRequestError::PrevTxOut(e)));
                    None
                }
            })
            .collect::<Vec<_>>();
        err?;

        if let Some(first) = input_scripts.first() {
            input_scripts.iter().try_for_each(|input_type| {
                if input_type != first {
                    Err(RequestError::from(InternalRequestError::MixedInputScripts(
                        *first,
                        *input_type,
                    )))
                } else {
                    Ok(())
                }
            })?;
        }

        Ok(MaybeInputsSeen { psbt: self.psbt, params: self.params })
    }
}

/// Typestate to validate that the Original PSBT has no inputs that have been seen before.
///
/// Call [`check_no_inputs_seen`](struct.MaybeInputsSeen.html#method.check_no_inputs_seen_before) to proceed.
#[derive(Clone)]
pub struct MaybeInputsSeen {
    psbt: Psbt,
    params: Params,
}
impl MaybeInputsSeen {
    /// Make sure that the original transaction inputs have never been seen before.
    /// This prevents probing attacks. This prevents reentrant Payjoin, where a sender
    /// proposes a Payjoin PSBT as a new Original PSBT for a new Payjoin.
    pub fn check_no_inputs_seen_before(
        self,
        is_known: impl Fn(&OutPoint) -> Result<bool, Error>,
    ) -> Result<OutputsUnknown, Error> {
        self.psbt.input_pairs().try_for_each(|input| {
            match is_known(&input.txin.previous_output) {
                Ok(false) => Ok::<(), Error>(()),
                Ok(true) =>  {
                    log::warn!("Request contains an input we've seen before: {}. Preventing possible probing attack.", input.txin.previous_output);
                    Err(Error::BadRequest(
                        InternalRequestError::InputSeen(input.txin.previous_output).into(),
                    ))?
                },
                Err(e) => Err(Error::Server(e.into()))?,
            }
        })?;

        Ok(OutputsUnknown { psbt: self.psbt, params: self.params })
    }
}

/// The receiver has not yet identified which outputs belong to the receiver.
///
/// Only accept PSBTs that send us money.
/// Identify those outputs with `identify_receiver_outputs()` to proceed
#[derive(Clone)]
pub struct OutputsUnknown {
    psbt: Psbt,
    params: Params,
}

impl OutputsUnknown {
    /// Find which outputs belong to the receiver
    pub fn identify_receiver_outputs(
        self,
        is_receiver_output: impl Fn(&Script) -> Result<bool, Error>,
    ) -> Result<WantsOutputs, Error> {
        let owned_vouts: Vec<usize> = self
            .psbt
            .unsigned_tx
            .output
            .iter()
            .enumerate()
            .filter_map(|(vout, txo)| match is_receiver_output(&txo.script_pubkey) {
                Ok(true) => Some(Ok(vout)),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<Vec<_>, _>>()?;

        if owned_vouts.is_empty() {
            return Err(Error::BadRequest(InternalRequestError::MissingPayment.into()));
        }

        Ok(WantsOutputs {
            original_psbt: self.psbt.clone(),
            payjoin_psbt: self.psbt,
            params: self.params,
            owned_vouts,
        })
    }
}

/// A checked proposal that the receiver may substitute or add outputs to
#[derive(Debug, Clone)]
pub struct WantsOutputs {
    original_psbt: Psbt,
    payjoin_psbt: Psbt,
    params: Params,
    owned_vouts: Vec<usize>,
}

impl WantsOutputs {
    pub fn is_output_substitution_disabled(&self) -> bool {
        self.params.disable_output_substitution
    }

    /// If output substitution is enabled, replace the receiver's output script with a new one.
    pub fn try_substitute_receiver_output(
        self,
        generate_script: impl Fn() -> Result<bitcoin::ScriptBuf, Error>,
    ) -> Result<WantsInputs, Error> {
        let output_value = self.payjoin_psbt.unsigned_tx.output[self.owned_vouts[0]].value;
        let outputs = vec![TxOut { value: output_value, script_pubkey: generate_script()? }];
        self.try_substitute_receiver_outputs(Some(outputs))
    }

    pub fn try_substitute_receiver_outputs(
        self,
        outputs: Option<Vec<TxOut>>,
    ) -> Result<WantsInputs, Error> {
        let mut payjoin_psbt = self.payjoin_psbt.clone();
        match outputs {
            Some(o) => {
                if self.params.disable_output_substitution {
                    // TODO: only fail if the original output's amount decreased or its script pubkey is not in `outputs`
                    return Err(Error::Server("Output substitution is disabled.".into()));
                }
                let mut replacement_outputs = o.into_iter();
                let mut outputs = vec![];
                for (i, output) in self.payjoin_psbt.unsigned_tx.output.iter().enumerate() {
                    if self.owned_vouts.contains(&i) {
                        // Receiver output: substitute with a provided output
                        // TODO: pick from outputs in random order?
                        outputs.push(
                            replacement_outputs
                                .next()
                                .ok_or(Error::Server("Not enough outputs".into()))?,
                        );
                    } else {
                        // Sender output: leave it as is
                        outputs.push(output.clone());
                    }
                }
                // Append all remaining outputs
                outputs.extend(replacement_outputs);
                payjoin_psbt.unsigned_tx.output = outputs;
                // TODO: update self.owned_vouts?
            }
            None => log::info!("No outputs provided: skipping output substitution."),
        }
        Ok(WantsInputs {
            original_psbt: self.original_psbt,
            payjoin_psbt,
            params: self.params,
            owned_vouts: self.owned_vouts,
        })
    }
}

/// A checked proposal that the receiver may contribute inputs to to make a payjoin
#[derive(Debug, Clone)]
pub struct WantsInputs {
    original_psbt: Psbt,
    payjoin_psbt: Psbt,
    params: Params,
    owned_vouts: Vec<usize>,
}

impl WantsInputs {
    /// Select receiver input such that the payjoin avoids surveillance.
    /// Return the input chosen that has been applied to the Proposal.
    ///
    /// Proper coin selection allows payjoin to resemble ordinary transactions.
    /// To ensure the resemblance, a number of heuristics must be avoided.
    ///
    /// UIH "Unnecessary input heuristic" is avoided for two-output transactions.
    /// A simple consolidation is otherwise chosen if available.
    pub fn try_preserving_privacy(
        &self,
        candidate_inputs: HashMap<Amount, OutPoint>,
    ) -> Result<Vec<OutPoint>, SelectionError> {
        if candidate_inputs.is_empty() {
            return Err(SelectionError::from(InternalSelectionError::Empty));
        }

        if self.payjoin_psbt.outputs.len() > 2 {
            // This doesn't attempt to preserve privacy...
            self.do_coin_selection(candidate_inputs)
        } else if self.payjoin_psbt.outputs.len() == 2 {
            self.avoid_uih(candidate_inputs)
        } else {
            self.select_first_candidate(candidate_inputs)
        }
    }

    fn do_coin_selection(
        &self,
        candidate_inputs: HashMap<Amount, OutPoint>,
    ) -> Result<Vec<OutPoint>, SelectionError> {
        // Calculate the amount that the receiver must contribute
        let output_amount =
            self.payjoin_psbt.unsigned_tx.output.iter().fold(0, |acc, output| acc + output.value);
        let original_output_amount =
            self.original_psbt.unsigned_tx.output.iter().fold(0, |acc, output| acc + output.value);
        let min_input_amount = min(0, output_amount - original_output_amount);

        // Select inputs that can pay for that amount
        // TODO: use a better coin selection algorithm
        let mut selected_coins = vec![];
        let mut input_sats = 0;
        for candidate in candidate_inputs {
            let candidate_sats = candidate.0.to_sat();
            selected_coins.push(candidate.1);
            input_sats += candidate_sats;

            if input_sats >= min_input_amount {
                return Ok(selected_coins);
            }
        }

        Err(SelectionError::from(InternalSelectionError::CannotAfford))
    }

    /// UIH "Unnecessary input heuristic" is one class of heuristics to avoid. We define
    /// UIH1 and UIH2 according to the BlockSci practice
    /// BlockSci UIH1 and UIH2:
    // if min(in) > min(out) then UIH1 else UIH2
    // https://eprint.iacr.org/2022/589.pdf
    fn avoid_uih(
        &self,
        candidate_inputs: HashMap<Amount, OutPoint>,
    ) -> Result<Vec<OutPoint>, SelectionError> {
        let min_original_out_sats = self
            .payjoin_psbt
            .unsigned_tx
            .output
            .iter()
            .map(|output| output.value)
            .min()
            .unwrap_or_else(|| Amount::MAX_MONEY);

        let min_original_in_sats = self
            .payjoin_psbt
            .input_pairs()
            .filter_map(|input| input.previous_txout().ok().map(|txo| txo.value))
            .min()
            .unwrap_or_else(|| Amount::MAX_MONEY);

        let prior_payment_sats = self.payjoin_psbt.unsigned_tx.output[self.owned_vouts[0]].value;

        for candidate in candidate_inputs {
            let candidate_sats = candidate.0;
            let candidate_min_out = min(min_original_out_sats, prior_payment_sats + candidate_sats);
            let candidate_min_in = min(min_original_in_sats, candidate_sats);

            if candidate_min_in > candidate_min_out {
                // The candidate avoids UIH2 but conforms to UIH1: Optimal change heuristic.
                // It implies the smallest output is the sender's change address.
                return Ok(vec![candidate.1]);
            }
        }

        // No suitable privacy preserving selection found
        Err(SelectionError::from(InternalSelectionError::NotFound))
    }

    fn select_first_candidate(
        &self,
        candidate_inputs: HashMap<Amount, OutPoint>,
    ) -> Result<Vec<OutPoint>, SelectionError> {
        match candidate_inputs.values().next().cloned() {
            Some(outpoint) => Ok(vec![outpoint]),
            None => Err(SelectionError::from(InternalSelectionError::NotFound)),
        }
    }

    pub fn contribute_witness_input(self, txo: TxOut, outpoint: OutPoint) -> ProvisionalProposal {
        let mut payjoin_psbt = self.payjoin_psbt.clone();
        // The payjoin proposal must not introduce mixed input sequence numbers
        let original_sequence = self
            .payjoin_psbt
            .unsigned_tx
            .input
            .first()
            .map(|input| input.sequence)
            .unwrap_or_default();

        // Add the value of new receiver input to receiver output
        let txo_value = txo.value;
        let vout_to_augment =
            self.owned_vouts.choose(&mut rand::thread_rng()).expect("owned_vouts is empty");
        payjoin_psbt.unsigned_tx.output[*vout_to_augment].value += txo_value;

        // Insert contribution at random index for privacy
        let mut rng = rand::thread_rng();
        let index = rng.gen_range(0..=self.payjoin_psbt.unsigned_tx.input.len());
        payjoin_psbt
            .inputs
            .insert(index, bitcoin::psbt::Input { witness_utxo: Some(txo), ..Default::default() });
        payjoin_psbt.unsigned_tx.input.insert(
            index,
            bitcoin::TxIn {
                previous_output: outpoint,
                sequence: original_sequence,
                ..Default::default()
            },
        );
        ProvisionalProposal {
            original_psbt: self.original_psbt,
            payjoin_psbt,
            params: self.params,
            owned_vouts: self.owned_vouts,
        }
    }

    pub fn contribute_non_witness_input(
        self,
        tx: bitcoin::Transaction,
        outpoint: OutPoint,
    ) -> ProvisionalProposal {
        let mut payjoin_psbt = self.payjoin_psbt.clone();
        // The payjoin proposal must not introduce mixed input sequence numbers
        let original_sequence = self
            .payjoin_psbt
            .unsigned_tx
            .input
            .first()
            .map(|input| input.sequence)
            .unwrap_or_default();

        // Add the value of new receiver input to receiver output
        let txo_value = tx.output[outpoint.vout as usize].value;
        let vout_to_augment =
            self.owned_vouts.choose(&mut rand::thread_rng()).expect("owned_vouts is empty");
        payjoin_psbt.unsigned_tx.output[*vout_to_augment].value += txo_value;

        // Insert contribution at random index for privacy
        let mut rng = rand::thread_rng();
        let index = rng.gen_range(0..=self.payjoin_psbt.unsigned_tx.input.len());

        // Add the new input to the PSBT
        payjoin_psbt.inputs.insert(
            index,
            bitcoin::psbt::Input { non_witness_utxo: Some(tx), ..Default::default() },
        );
        payjoin_psbt.unsigned_tx.input.insert(
            index,
            bitcoin::TxIn {
                previous_output: outpoint,
                sequence: original_sequence,
                ..Default::default()
            },
        );
        ProvisionalProposal {
            original_psbt: self.original_psbt,
            payjoin_psbt,
            params: self.params,
            owned_vouts: self.owned_vouts,
        }
    }

    // TODO: temporary workaround
    fn skip_contribute_inputs(self) -> ProvisionalProposal {
        ProvisionalProposal {
            original_psbt: self.original_psbt,
            payjoin_psbt: self.payjoin_psbt,
            params: self.params,
            owned_vouts: self.owned_vouts,
        }
    }
}

/// A checked proposal that the receiver may sign and finalize to make a proposal PSBT that the
/// sender will accept.
#[derive(Debug, Clone)]
pub struct ProvisionalProposal {
    original_psbt: Psbt,
    payjoin_psbt: Psbt,
    params: Params,
    owned_vouts: Vec<usize>,
}

impl ProvisionalProposal {
    /// Apply additional fee contribution now that the receiver has contributed input
    /// this is kind of a "build_proposal" step before we sign and finalize and extract
    ///
    /// WARNING: DO NOT ALTER INPUTS OR OUTPUTS AFTER THIS STEP
    fn apply_fee(&mut self, min_feerate: Option<FeeRate>) -> Result<&Psbt, RequestError> {
        let min_feerate = min_feerate.unwrap_or(FeeRate::MIN);
        log::trace!("min_feerate: {:?}", min_feerate);
        log::trace!("params.min_feerate: {:?}", self.params.min_feerate);
        let min_feerate = max(min_feerate, self.params.min_feerate);
        log::debug!("min_feerate: {:?}", min_feerate);

        // this error should never happen. We check for at least one input in the constructor
        let input_pair = self
            .payjoin_psbt
            .input_pairs()
            .next()
            .ok_or(InternalRequestError::OriginalPsbtNotBroadcastable)?;
        let txo = input_pair.previous_txout().map_err(InternalRequestError::PrevTxOut)?;
        let input_type = InputType::from_spent_input(txo, &self.payjoin_psbt.inputs[0])
            .map_err(InternalRequestError::InputType)?;
        let contribution_weight = input_type.expected_input_weight();
        log::trace!("contribution_weight: {}", contribution_weight);
        let mut additional_fee = contribution_weight * min_feerate;
        let max_additional_fee_contribution =
            self.params.additional_fee_contribution.unwrap_or_default().0;
        if additional_fee >= max_additional_fee_contribution {
            // Cap fee at the sender's contribution to simplify this method
            additional_fee = max_additional_fee_contribution;
        }
        log::trace!("additional_fee: {}", additional_fee);
        if additional_fee > bitcoin::Amount::ZERO {
            log::trace!(
                "self.params.additional_fee_contribution: {:?}",
                self.params.additional_fee_contribution
            );
            if let Some((_, additional_fee_output_index)) = self.params.additional_fee_contribution
            {
                if !self.owned_vouts.contains(&additional_fee_output_index) {
                    // remove additional miner fee from the sender's specified output
                    self.payjoin_psbt.unsigned_tx.output[additional_fee_output_index].value -=
                        additional_fee;
                }
            }
        }
        Ok(&self.payjoin_psbt)
    }

    /// Return a Payjoin Proposal PSBT that the sender will find acceptable.
    ///
    /// This attempts to calculate any network fee owed by the receiver, subtract it from their output,
    /// and return a PSBT that can produce a consensus-valid transaction that the sender will accept.
    ///
    /// wallet_process_psbt should sign and finalize receiver inputs
    fn prepare_psbt(mut self, processed_psbt: Psbt) -> Result<PayjoinProposal, RequestError> {
        self.payjoin_psbt = processed_psbt;
        log::trace!("Preparing PSBT {:#?}", self.payjoin_psbt);
        for output in self.payjoin_psbt.outputs_mut() {
            output.bip32_derivation.clear();
            output.tap_key_origins.clear();
            output.tap_internal_key = None;
        }
        for input in self.payjoin_psbt.inputs_mut() {
            input.bip32_derivation.clear();
            input.tap_key_origins.clear();
            input.tap_internal_key = None;
            input.partial_sigs.clear();
        }
        for i in self.sender_input_indexes() {
            log::trace!("Clearing sender input {}", i);
            self.payjoin_psbt.inputs[i].non_witness_utxo = None;
            self.payjoin_psbt.inputs[i].witness_utxo = None;
            self.payjoin_psbt.inputs[i].final_script_sig = None;
            self.payjoin_psbt.inputs[i].final_script_witness = None;
            self.payjoin_psbt.inputs[i].tap_key_sig = None;
        }

        Ok(PayjoinProposal {
            payjoin_psbt: self.payjoin_psbt,
            owned_vouts: self.owned_vouts,
            params: self.params,
        })
    }

    fn sender_input_indexes(&self) -> Vec<usize> {
        // iterate proposal as mutable WITH the outpoint (previous_output) available too
        let mut original_inputs = self.original_psbt.input_pairs().peekable();
        let mut sender_input_indexes = vec![];
        for (i, input) in self.payjoin_psbt.input_pairs().enumerate() {
            if let Some(original) = original_inputs.peek() {
                log::trace!(
                    "match previous_output: {} == {}",
                    input.txin.previous_output,
                    original.txin.previous_output
                );
                if input.txin.previous_output == original.txin.previous_output {
                    sender_input_indexes.push(i);
                    original_inputs.next();
                }
            }
        }
        sender_input_indexes
    }

    pub fn finalize_proposal(
        mut self,
        wallet_process_psbt: impl Fn(&Psbt) -> Result<Psbt, Error>,
        min_feerate_sat_per_vb: Option<FeeRate>,
    ) -> Result<PayjoinProposal, Error> {
        for i in self.sender_input_indexes() {
            log::trace!("Clearing sender script signatures for input {}", i);
            self.payjoin_psbt.inputs[i].final_script_sig = None;
            self.payjoin_psbt.inputs[i].final_script_witness = None;
            self.payjoin_psbt.inputs[i].tap_key_sig = None;
        }
        let psbt = self.apply_fee(min_feerate_sat_per_vb)?;
        let psbt = wallet_process_psbt(psbt)?;
        let payjoin_proposal = self.prepare_psbt(psbt)?;
        Ok(payjoin_proposal)
    }
}

/// A mutable checked proposal that the receiver may contribute inputs to to make a payjoin.
#[derive(Clone)]
pub struct PayjoinProposal {
    payjoin_psbt: Psbt,
    params: Params,
    owned_vouts: Vec<usize>,
}

impl PayjoinProposal {
    pub fn utxos_to_be_locked(&self) -> impl '_ + Iterator<Item = &bitcoin::OutPoint> {
        self.payjoin_psbt.unsigned_tx.input.iter().map(|input| &input.previous_output)
    }

    pub fn is_output_substitution_disabled(&self) -> bool {
        self.params.disable_output_substitution
    }

    pub fn owned_vouts(&self) -> &Vec<usize> { &self.owned_vouts }

    pub fn psbt(&self) -> &Psbt { &self.payjoin_psbt }
}

#[cfg(test)]
mod test {
    use super::*;

    struct MockHeaders {
        length: String,
    }

    impl MockHeaders {
        #[cfg(test)]
        fn new(length: u64) -> MockHeaders { MockHeaders { length: length.to_string() } }
    }

    impl Headers for MockHeaders {
        fn get_header(&self, key: &str) -> Option<&str> {
            match key {
                "content-length" => Some(&self.length),
                "content-type" => Some("text/plain"),
                _ => None,
            }
        }
    }

    fn proposal_from_test_vector() -> Result<UncheckedProposal, RequestError> {
        // OriginalPSBT Test Vector from BIP
        // | InputScriptType | Orginal PSBT Fee rate | maxadditionalfeecontribution | additionalfeeoutputindex|
        // |-----------------|-----------------------|------------------------------|-------------------------|
        // | P2SH-P2WPKH     |  2 sat/vbyte          | 0.00000182                   | 0                       |
        let original_psbt = "cHNidP8BAHMCAAAAAY8nutGgJdyYGXWiBEb45Hoe9lWGbkxh/6bNiOJdCDuDAAAAAAD+////AtyVuAUAAAAAF6kUHehJ8GnSdBUOOv6ujXLrWmsJRDCHgIQeAAAAAAAXqRR3QJbbz0hnQ8IvQ0fptGn+votneofTAAAAAAEBIKgb1wUAAAAAF6kU3k4ekGHKWRNbA1rV5tR5kEVDVNCHAQcXFgAUx4pFclNVgo1WWAdN1SYNX8tphTABCGsCRzBEAiB8Q+A6dep+Rz92vhy26lT0AjZn4PRLi8Bf9qoB/CMk0wIgP/Rj2PWZ3gEjUkTlhDRNAQ0gXwTO7t9n+V14pZ6oljUBIQMVmsAaoNWHVMS02LfTSe0e388LNitPa1UQZyOihY+FFgABABYAFEb2Giu6c4KO5YW0pfw3lGp9jMUUAAA=";

        let body = original_psbt.as_bytes();
        let headers = MockHeaders::new(body.len() as u64);
        UncheckedProposal::from_request(
            body,
            "maxadditionalfeecontribution=182&additionalfeeoutputindex=0",
            headers,
        )
    }

    #[test]
    fn can_get_proposal_from_request() {
        let proposal = proposal_from_test_vector();
        assert!(proposal.is_ok(), "OriginalPSBT should be a valid request");
    }

    #[test]
    fn unchecked_proposal_unlocks_after_checks() {
        use std::str::FromStr;

        use bitcoin::{Address, Network};

        let proposal = proposal_from_test_vector().unwrap();
        assert_eq!(proposal.psbt_fee_rate().unwrap().to_sat_per_vb_floor(), 2);
        let mut payjoin = proposal
            .assume_interactive_receiver()
            .check_inputs_not_owned(|_| Ok(false))
            .expect("No inputs should be owned")
            .check_no_mixed_input_scripts()
            .expect("No mixed input scripts")
            .check_no_inputs_seen_before(|_| Ok(false))
            .expect("No inputs should be seen before")
            .identify_receiver_outputs(|script| {
                let network = Network::Bitcoin;
                Ok(Address::from_script(script, network).unwrap()
                    == Address::from_str(&"3CZZi7aWFugaCdUCS15dgrUUViupmB8bVM")
                        .unwrap()
                        .require_network(network)
                        .unwrap())
            })
            .expect("Receiver output should be identified")
            .try_substitute_receiver_outputs(None)
            .expect("Substitute outputs should do nothing")
            .skip_contribute_inputs(); // TODO: temporary workaround

        let payjoin = payjoin.apply_fee(None);

        assert!(payjoin.is_ok(), "Payjoin should be a valid PSBT");
    }
}
