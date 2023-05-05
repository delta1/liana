use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use iced::Command;
use liana::{
    descriptors::LianaDescriptor,
    miniscript::bitcoin::{
        self, util::psbt::Psbt, Address, Amount, Denomination, Network, OutPoint,
    },
};

use liana_ui::{
    component::{form, modal},
    widget::Element,
};

use crate::{
    app::{cache::Cache, error::Error, message::Message, state::psbt, view, wallet::Wallet},
    daemon::{
        model::{remaining_sequence, Coin, SpendTx},
        Daemon,
    },
};

/// See: https://github.com/wizardsardine/liana/blob/master/src/commands/mod.rs#L32
const DUST_OUTPUT_SATS: u64 = 5_000;

#[derive(Default, Clone)]
pub struct TransactionDraft {
    inputs: Vec<Coin>,
    generated: Option<Psbt>,
}

pub trait Step {
    fn view<'a>(&'a self, cache: &'a Cache) -> Element<'a, view::Message>;
    fn update(
        &mut self,
        daemon: Arc<dyn Daemon + Sync + Send>,
        cache: &Cache,
        message: Message,
    ) -> Command<Message>;
    fn apply(&self, _draft: &mut TransactionDraft) {}
    fn load(&mut self, _draft: &TransactionDraft) {}
}

pub struct DefineSpend {
    balance_available: Amount,
    recipients: Vec<Recipient>,
    is_valid: bool,
    is_duplicate: bool,

    descriptor: LianaDescriptor,
    timelock: u16,
    coins: Vec<(Coin, bool)>,
    amount_left_to_select: Option<Amount>,
    feerate: form::Value<String>,
    generated: Option<Psbt>,
    warning: Option<Error>,
}

impl DefineSpend {
    pub fn new(
        descriptor: LianaDescriptor,
        coins: Vec<Coin>,
        timelock: u16,
        blockheight: u32,
    ) -> Self {
        let balance_available = coins
            .iter()
            .filter_map(|coin| {
                if coin.spend_info.is_none() {
                    Some(coin.amount)
                } else {
                    None
                }
            })
            .sum();
        let mut coins: Vec<(Coin, bool)> = coins
            .into_iter()
            .filter_map(|c| {
                if c.spend_info.is_none() {
                    Some((c, false))
                } else {
                    None
                }
            })
            .collect();
        coins.sort_by(|(a, _), (b, _)| {
            if remaining_sequence(a, blockheight, timelock)
                == remaining_sequence(b, blockheight, timelock)
            {
                // bigger amount first
                b.amount.cmp(&a.amount)
            } else {
                // smallest blockheight (remaining_sequence) first
                a.block_height.cmp(&b.block_height)
            }
        });
        Self {
            balance_available,
            descriptor,
            timelock,
            generated: None,
            coins,
            recipients: vec![Recipient::default()],
            is_valid: false,
            is_duplicate: false,
            feerate: form::Value::default(),
            amount_left_to_select: None,
            warning: None,
        }
    }
    fn check_valid(&mut self) {
        self.is_valid = self.feerate.valid && !self.feerate.value.is_empty();
        self.is_duplicate = false;
        if !self.coins.iter().any(|(_, selected)| *selected) {
            self.is_valid = false;
        }
        for (i, recipient) in self.recipients.iter().enumerate() {
            if !recipient.valid() {
                self.is_valid = false;
            }
            if !self.is_duplicate && !recipient.address.value.is_empty() {
                self.is_duplicate = self.recipients[..i]
                    .iter()
                    .any(|r| r.address.value == recipient.address.value);
            }
        }
    }
    fn amount_left_to_select(&mut self) {
        // We need the feerate in order to compute the required amount of BTC to
        // select. Return early if we don't to not do unnecessary computation.
        let feerate = match self.feerate.value.parse::<u64>() {
            Ok(f) => f,
            Err(_) => {
                self.amount_left_to_select = None;
                return;
            }
        };

        // The coins to be included in this transaction.
        let selected_coins: Vec<_> = self
            .coins
            .iter()
            .filter_map(|(c, selected)| if *selected { Some(c) } else { None })
            .collect();

        // A dummy representation of the transaction that will be computed, for
        // the purpose of computing its size in order to anticipate the fees needed.
        // NOTE: we make the conservative estimation a change output will always be
        // needed.
        let tx_template = bitcoin::Transaction {
            version: 2,
            lock_time: bitcoin::PackedLockTime(0),
            input: selected_coins
                .iter()
                .map(|_| bitcoin::TxIn::default())
                .collect(),
            output: self
                .recipients
                .iter()
                .filter_map(|recipient| {
                    if recipient.valid() {
                        Some(bitcoin::TxOut {
                            script_pubkey: Address::from_str(&recipient.address.value)
                                .unwrap()
                                .script_pubkey(),
                            value: recipient.amount().unwrap(),
                        })
                    } else {
                        None
                    }
                })
                .collect(),
        };
        // nValue size + scriptPubKey CompactSize + OP_0 + PUSH32 + <wit program>
        const CHANGE_TXO_SIZE: usize = 8 + 1 + 1 + 1 + 32;
        let satisfaction_vsize = self.descriptor.max_sat_weight() / 4;
        let transaction_size =
            tx_template.vsize() + satisfaction_vsize * tx_template.input.len() + CHANGE_TXO_SIZE;

        // Now the calculation of the amount left to be selected by the user is a simple
        // substraction between the value needed by the transaction to be created and the
        // value that was selected already.
        let selected_amount = selected_coins.iter().map(|c| c.amount.to_sat()).sum();
        let output_sum: u64 = tx_template.output.iter().map(|o| o.value).sum();
        let needed_amount: u64 = transaction_size as u64 * feerate + output_sum;
        self.amount_left_to_select = Some(Amount::from_sat(
            needed_amount.saturating_sub(selected_amount),
        ));
    }
}

impl Step for DefineSpend {
    fn update(
        &mut self,
        daemon: Arc<dyn Daemon + Sync + Send>,
        cache: &Cache,
        message: Message,
    ) -> Command<Message> {
        if let Message::View(view::Message::CreateSpend(msg)) = message {
            match msg {
                view::CreateSpendMessage::AddRecipient => {
                    self.recipients.push(Recipient::default());
                }
                view::CreateSpendMessage::DeleteRecipient(i) => {
                    self.recipients.remove(i);
                }
                view::CreateSpendMessage::RecipientEdited(i, _, _) => {
                    self.recipients
                        .get_mut(i)
                        .unwrap()
                        .update(cache.network, msg);
                }

                view::CreateSpendMessage::FeerateEdited(s) => {
                    if let Ok(value) = s.parse::<u64>() {
                        self.feerate.value = s;
                        self.feerate.valid = value != 0;
                        self.amount_left_to_select();
                    } else if s.is_empty() {
                        self.feerate.value = "".to_string();
                        self.feerate.valid = true;
                        self.amount_left_to_select = None;
                    } else {
                        self.feerate.valid = false;
                        self.amount_left_to_select = None;
                    }
                    self.warning = None;
                }
                view::CreateSpendMessage::Generate => {
                    let inputs: Vec<OutPoint> = self
                        .coins
                        .iter()
                        .filter_map(
                            |(coin, selected)| if *selected { Some(coin.outpoint) } else { None },
                        )
                        .collect();
                    let mut outputs: HashMap<Address, u64> = HashMap::new();
                    for recipient in &self.recipients {
                        outputs.insert(
                            Address::from_str(&recipient.address.value).expect("Checked before"),
                            recipient.amount().expect("Checked before"),
                        );
                    }
                    let feerate_vb = self.feerate.value.parse::<u64>().unwrap_or(0);
                    self.warning = None;
                    return Command::perform(
                        async move {
                            daemon
                                .create_spend_tx(&inputs, &outputs, feerate_vb)
                                .map(|res| res.psbt)
                                .map_err(|e| e.into())
                        },
                        Message::Psbt,
                    );
                }
                view::CreateSpendMessage::SelectCoin(i) => {
                    if let Some(coin) = self.coins.get_mut(i) {
                        coin.1 = !coin.1;
                        self.amount_left_to_select();
                    }
                }
                _ => {}
            }
            self.check_valid();
            Command::none()
        } else {
            if let Message::Psbt(res) = message {
                match res {
                    Ok(psbt) => {
                        self.generated = Some(psbt);
                        return Command::perform(async {}, |_| Message::View(view::Message::Next));
                    }
                    Err(e) => self.warning = Some(e),
                }
            }
            Command::none()
        }
    }

    fn apply(&self, draft: &mut TransactionDraft) {
        draft.inputs = self
            .coins
            .iter()
            .filter_map(|(coin, selected)| if *selected { Some(*coin) } else { None })
            .collect();
        draft.generated = self.generated.clone();
    }

    fn view<'a>(&'a self, cache: &'a Cache) -> Element<'a, view::Message> {
        view::spend::create_spend_tx(
            cache,
            &self.balance_available,
            self.recipients
                .iter()
                .enumerate()
                .map(|(i, recipient)| recipient.view(i).map(view::Message::CreateSpend))
                .collect(),
            Amount::from_sat(
                self.recipients
                    .iter()
                    .map(|r| r.amount().unwrap_or(0_u64))
                    .sum(),
            ),
            self.is_valid,
            self.is_duplicate,
            self.timelock,
            &self.coins,
            self.amount_left_to_select.as_ref(),
            &self.feerate,
            self.warning.as_ref(),
        )
    }
}

#[derive(Default)]
struct Recipient {
    address: form::Value<String>,
    amount: form::Value<String>,
}

impl Recipient {
    fn amount(&self) -> Result<u64, Error> {
        if self.amount.value.is_empty() {
            return Err(Error::Unexpected("Amount should be non-zero".to_string()));
        }

        let amount = Amount::from_str_in(&self.amount.value, Denomination::Bitcoin)
            .map_err(|_| Error::Unexpected("cannot parse output amount".to_string()))?;

        if amount.to_sat() == 0 {
            return Err(Error::Unexpected("Amount should be non-zero".to_string()));
        }

        if amount.to_sat() < DUST_OUTPUT_SATS {
            return Err(Error::Unexpected("Amount should be non-zero".to_string()));
        }

        if let Ok(address) = Address::from_str(&self.address.value) {
            if amount <= address.script_pubkey().dust_value() {
                return Err(Error::Unexpected(
                    "Amount must be superior to script dust value".to_string(),
                ));
            }
        }

        Ok(amount.to_sat())
    }

    fn valid(&self) -> bool {
        !self.address.value.is_empty()
            && self.address.valid
            && !self.amount.value.is_empty()
            && self.amount.valid
    }

    fn update(&mut self, network: Network, message: view::CreateSpendMessage) {
        match message {
            view::CreateSpendMessage::RecipientEdited(_, "address", address) => {
                self.address.value = address;
                if let Ok(address) = Address::from_str(&self.address.value) {
                    self.address.valid = address.is_valid_for_network(network);
                    if !self.amount.value.is_empty() {
                        self.amount.valid = self.amount().is_ok();
                    }
                } else if self.address.value.is_empty() {
                    // Make the error disappear if we deleted the invalid address
                    self.address.valid = true;
                } else {
                    self.address.valid = false;
                }
            }
            view::CreateSpendMessage::RecipientEdited(_, "amount", amount) => {
                self.amount.value = amount;
                if !self.amount.value.is_empty() {
                    self.amount.valid = self.amount().is_ok();
                } else {
                    // Make the error disappear if we deleted the invalid amount
                    self.amount.valid = true;
                }
            }
            _ => {}
        };
    }

    fn view(&self, i: usize) -> Element<view::CreateSpendMessage> {
        view::spend::recipient_view(i, &self.address, &self.amount)
    }
}

pub struct SaveSpend {
    wallet: Arc<Wallet>,
    spend: Option<psbt::PsbtState>,
}

impl SaveSpend {
    pub fn new(wallet: Arc<Wallet>) -> Self {
        Self {
            wallet,
            spend: None,
        }
    }
}

impl Step for SaveSpend {
    fn load(&mut self, draft: &TransactionDraft) {
        let psbt = draft.generated.clone().unwrap();
        let sigs = self
            .wallet
            .main_descriptor
            .partial_spend_info(&psbt)
            .unwrap();
        self.spend = Some(psbt::PsbtState::new(
            self.wallet.clone(),
            SpendTx::new(None, psbt, draft.inputs.clone(), sigs),
            false,
        ));
    }

    fn update(
        &mut self,
        daemon: Arc<dyn Daemon + Sync + Send>,
        cache: &Cache,
        message: Message,
    ) -> Command<Message> {
        if let Some(spend) = &mut self.spend {
            spend.update(daemon, cache, message)
        } else {
            Command::none()
        }
    }

    fn view<'a>(&'a self, cache: &'a Cache) -> Element<'a, view::Message> {
        let spend = self.spend.as_ref().unwrap();
        let content = view::spend::spend_view(
            cache,
            &spend.tx,
            spend.saved,
            &spend.desc_policy,
            &spend.wallet.keys_aliases,
            cache.network,
        );
        if let Some(action) = &spend.action {
            modal::Modal::new(content, action.view())
                .on_blur(Some(view::Message::Spend(view::SpendTxMessage::Cancel)))
                .into()
        } else {
            content
        }
    }
}
