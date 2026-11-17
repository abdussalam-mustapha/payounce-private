// SPDX-License-Identifier: MIT
//
// Payounce Private — a rotating savings circle on Tari Ootle.
//
// Every round each member pays the same contribution and one member takes the whole pot, in join
// order. Members lock collateral when they join; a missed payment is taken from that collateral,
// and a member whose collateral runs out is marked defaulted and forfeits any payout they have not
// received yet. Forfeited pots are shared among the members who kept paying.
//
// Privacy: contributions are revealed so the group can verify that everyone paid the agreed amount,
// but payouts can leave the circle as stealth outputs (`claim_private`), so nobody watching the
// chain can see who received the pot, how much, or link it to their wallet.

use tari_template_lib::prelude::*;

#[template]
mod template {
    use std::collections::HashMap;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Status {
        Open,
        Active,
        Completed,
        Cancelled,
    }

    #[derive(Debug, Clone)]
    pub struct Member {
        pub collateral: Amount,
        pub defaulted: bool,
    }

    /// Read-only snapshot returned by `state()`, so frontends need a single call.
    #[derive(Debug, Clone)]
    pub struct CircleState {
        pub name: String,
        pub status: Status,
        pub contribution: Amount,
        pub collateral: Amount,
        pub max_members: u32,
        pub epochs_per_round: u64,
        pub join_deadline_epoch: u64,
        pub start_epoch: u64,
        pub current_round: u32,
        pub round_deadline_epoch: u64,
        pub current_epoch: u64,
        pub pot: Amount,
        pub members: Vec<RistrettoPublicKeyBytes>,
        pub paid_this_round: Vec<RistrettoPublicKeyBytes>,
        pub defaulted: Vec<RistrettoPublicKeyBytes>,
    }

    pub struct PayounceCircle {
        name: String,
        contribution: Amount,
        collateral: Amount,
        max_members: u32,
        epochs_per_round: u64,
        join_deadline_epoch: u64,

        status: Status,
        start_epoch: u64,
        current_round: u32,
        pot: Amount,
        forfeit_pool: Amount,
        active_count: u32,

        /// Payout order = join order.
        members: Vec<RistrettoPublicKeyBytes>,
        member_info: HashMap<RistrettoPublicKeyBytes, Member>,
        paid_this_round: Vec<RistrettoPublicKeyBytes>,
        claimable: HashMap<RistrettoPublicKeyBytes, Amount>,

        /// Holds all collateral, contributions and unclaimed payouts (revealed TARI).
        vault: Vault,
    }

    impl PayounceCircle {
        /// Creates a circle. Nobody owns it: the rules below are the only way to move funds.
        ///
        /// * `contribution` – TARI (micro-units, 1 TARI = 1_000_000) each member pays per round
        /// * `collateral` – TARI each member locks when joining
        /// * `max_members` – 2..=50, also the number of rounds
        /// * `epochs_per_round` – round length in Ootle epochs
        /// * `join_epochs` – how long the circle stays open for joining
        pub fn new(
            name: String,
            contribution: Amount,
            collateral: Amount,
            max_members: u32,
            epochs_per_round: u64,
            join_epochs: u64,
        ) -> Component<Self> {
            assert!(!name.is_empty() && name.len() <= 64, "Name must be 1-64 characters");
            assert!(contribution.is_positive(), "Contribution must be positive");
            assert!((2..=50).contains(&max_members), "Circle needs 2-50 members");
            assert!(epochs_per_round > 0, "Round length must be at least one epoch");
            assert!(join_epochs > 0, "Join window must be at least one epoch");

            let state = Self {
                name,
                contribution,
                collateral,
                max_members,
                epochs_per_round,
                join_deadline_epoch: Consensus::current_epoch() + join_epochs,
                status: Status::Open,
                start_epoch: 0,
                current_round: 0,
                pot: Amount::ZERO,
                forfeit_pool: Amount::ZERO,
                active_count: 0,
                members: Vec::new(),
                member_info: HashMap::new(),
                paid_this_round: Vec::new(),
                claimable: HashMap::new(),
                vault: Vault::new_empty(TARI_TOKEN),
            };

            Component::new(state)
                .with_access_rules(AccessRules::allow_all())
                .with_owner_rule(OwnerRule::None)
                .create()
        }

        // ------------------------------------------------------------------
        // Joining
        // ------------------------------------------------------------------

        /// Join with exactly `collateral` TARI (revealed). The circle starts once it is full.
        pub fn join(&mut self, payment: Bucket) {
            assert!(self.status == Status::Open, "Circle is not open for joining");
            assert!(Consensus::current_epoch() <= self.join_deadline_epoch, "Joining has closed");
            let who = CallerContext::transaction_signer_public_key();
            assert!(!self.member_info.contains_key(&who), "Already a member");

            self.take_exact(payment, self.collateral);
            self.members.push(who);
            self.member_info.insert(who, Member { collateral: self.collateral, defaulted: false });
            self.active_count += 1;
            emit_event("Joined", metadata!["member" => who.to_string()]);

            if self.members.len() as u32 == self.max_members {
                self.status = Status::Active;
                self.start_epoch = Consensus::current_epoch();
                emit_event("Started", metadata!["epoch" => self.start_epoch.to_string()]);
            }
        }

        /// Leave before the circle starts and take your collateral back.
        pub fn leave(&mut self) -> Bucket {
            assert!(self.status == Status::Open, "Circle already started");
            let who = CallerContext::transaction_signer_public_key();
            let info = self.member_info.remove(&who).expect("Not a member");
            self.members.retain(|m| *m != who);
            self.active_count -= 1;
            emit_event("Left", metadata!["member" => who.to_string()]);
            self.vault.withdraw(info.collateral)
        }

        /// Anyone can cancel a circle that did not fill before the join deadline.
        /// Collateral becomes claimable by each member.
        pub fn cancel(&mut self) {
            assert!(self.status == Status::Open, "Circle is not open");
            assert!(Consensus::current_epoch() > self.join_deadline_epoch, "Join window still open");
            self.status = Status::Cancelled;
            for m in self.members.clone() {
                let info = self.member_info.get_mut(&m).unwrap();
                let amount = info.collateral;
                info.collateral = Amount::ZERO;
                self.credit(m, amount);
            }
            emit_event("Cancelled", metadata![]);
        }

        // ------------------------------------------------------------------
        // Rounds
        // ------------------------------------------------------------------

        /// Pay this round's contribution: exactly `contribution` TARI (revealed).
        pub fn contribute(&mut self, payment: Bucket) {
            assert!(self.status == Status::Active, "Circle is not active");
            let who = CallerContext::transaction_signer_public_key();
            let info = self.member_info.get(&who).expect("Not a member");
            assert!(!info.defaulted, "Member has defaulted");
            assert!(!self.paid_this_round.contains(&who), "Already paid this round");
            assert!(Consensus::current_epoch() <= self.round_deadline_epoch(), "Round has closed");

            self.take_exact(payment, self.contribution);
            self.paid_this_round.push(who);
            self.pot += self.contribution;
            emit_event("Contributed", metadata!["round" => self.current_round.to_string()]);
        }

        /// Close the current round: allowed once every active member has paid, or after the deadline.
        /// Missed payments are covered from collateral; the pot is credited to this round's recipient.
        pub fn settle_round(&mut self) {
            assert!(self.status == Status::Active, "Circle is not active");
            let all_paid = self.paid_this_round.len() as u32 >= self.active_count;
            assert!(
                all_paid || Consensus::current_epoch() > self.round_deadline_epoch(),
                "Round is still open"
            );

            if !all_paid {
                for m in self.members.clone() {
                    if self.paid_this_round.contains(&m) {
                        continue;
                    }
                    let info = self.member_info.get_mut(&m).unwrap();
                    if info.defaulted {
                        continue;
                    }
                    let taken = if info.collateral >= self.contribution {
                        info.collateral -= self.contribution;
                        self.contribution
                    } else {
                        let rest = info.collateral;
                        info.collateral = Amount::ZERO;
                        info.defaulted = true;
                        self.active_count -= 1;
                        rest
                    };
                    self.pot += taken;
                    emit_event(
                        "Slashed",
                        metadata!["round" => self.current_round.to_string(), "defaulted" => info.defaulted.to_string()],
                    );
                }
            }

            let recipient = self.members[self.current_round as usize];
            let amount = self.pot;
            let forfeited = self.member_info[&recipient].defaulted;
            if forfeited {
                self.forfeit_pool += amount;
            } else {
                self.credit(recipient, amount);
            }
            // The recipient is deliberately not included in the event, to keep payouts private.
            emit_event(
                "RoundSettled",
                metadata!["round" => self.current_round.to_string(), "forfeited" => forfeited.to_string()],
            );

            self.pot = Amount::ZERO;
            self.paid_this_round.clear();
            self.current_round += 1;
            if self.current_round == self.max_members {
                self.complete();
            }
        }

        fn complete(&mut self) {
            self.status = Status::Completed;
            let survivors: Vec<_> = self
                .members
                .iter()
                .copied()
                .filter(|m| !self.member_info[m].defaulted)
                .collect();

            if survivors.is_empty() {
                // Nobody kept paying; split evenly across all members as the least-bad option.
                let all = self.members.clone();
                self.share(&all, self.forfeit_pool);
            } else {
                self.share(&survivors, self.forfeit_pool);
            }
            self.forfeit_pool = Amount::ZERO;

            for m in self.members.clone() {
                let info = self.member_info.get_mut(&m).unwrap();
                let rest = info.collateral;
                info.collateral = Amount::ZERO;
                if rest.is_positive() {
                    self.credit(m, rest);
                }
            }
            emit_event("Completed", metadata![]);
        }

        // ------------------------------------------------------------------
        // Claiming
        // ------------------------------------------------------------------

        /// Claim everything owed to you as a revealed bucket (deposit it into your account in the same
        /// transaction).
        pub fn claim(&mut self) -> Bucket {
            let who = CallerContext::transaction_signer_public_key();
            let amount = self.claimable.remove(&who).expect("Nothing to claim");
            self.vault.withdraw(amount)
        }

        /// Claim everything owed to you as stealth outputs. The wallet builds `statement` so that its
        /// revealed input equals your claimable balance and its outputs are stealth UTXOs you control,
        /// hiding both the amount and the receiving wallet.
        pub fn claim_private(&mut self, statement: StealthTransferStatement) {
            let who = CallerContext::transaction_signer_public_key();
            let amount = self.claimable.remove(&who).expect("Nothing to claim");
            assert!(
                statement.inputs_statement.revealed_amount == amount,
                "Statement must spend exactly your claimable balance"
            );
            assert!(
                statement.outputs_statement.revealed_output.is_none(),
                "Private claims must not create a revealed output"
            );
            let funds = self.vault.withdraw(amount);
            funds.stealth_transfer(statement).drop_empty();
        }

        // ------------------------------------------------------------------
        // Views
        // ------------------------------------------------------------------

        pub fn state(&self) -> CircleState {
            CircleState {
                name: self.name.clone(),
                status: self.status,
                contribution: self.contribution,
                collateral: self.collateral,
                max_members: self.max_members,
                epochs_per_round: self.epochs_per_round,
                join_deadline_epoch: self.join_deadline_epoch,
                start_epoch: self.start_epoch,
                current_round: self.current_round,
                round_deadline_epoch: self.round_deadline_epoch(),
                current_epoch: Consensus::current_epoch(),
                pot: self.pot,
                members: self.members.clone(),
                paid_this_round: self.paid_this_round.clone(),
                defaulted: self
                    .members
                    .iter()
                    .copied()
                    .filter(|m| self.member_info[m].defaulted)
                    .collect(),
            }
        }

        /// Your own claimable balance. Only reveals the signer's balance, not anyone else's.
        pub fn my_claimable(&self) -> Amount {
            let who = CallerContext::transaction_signer_public_key();
            self.claimable.get(&who).copied().unwrap_or(Amount::ZERO)
        }

        // ------------------------------------------------------------------
        // Internal
        // ------------------------------------------------------------------

        fn round_deadline_epoch(&self) -> u64 {
            if self.status != Status::Active {
                return 0;
            }
            self.start_epoch + (self.current_round as u64 + 1) * self.epochs_per_round
        }

        fn take_exact(&mut self, payment: Bucket, expected: Amount) {
            assert!(payment.resource_address() == TARI_TOKEN, "Payment must be in TARI");
            assert!(payment.amount() == expected, "Payment must be exactly the required amount");
            self.vault.deposit(payment);
        }

        fn credit(&mut self, who: RistrettoPublicKeyBytes, amount: Amount) {
            *self.claimable.entry(who).or_insert(Amount::ZERO) += amount;
        }

        fn share(&mut self, to: &[RistrettoPublicKeyBytes], total: Amount) {
            if total.is_zero() || to.is_empty() {
                return;
            }
            let n = Amount::from(to.len() as u64);
            let each = total / n;
            let dust = total - each * n;
            for (i, m) in to.iter().enumerate() {
                let extra = if i == 0 { dust } else { Amount::ZERO };
                self.credit(*m, each + extra);
            }
        }
    }
}
