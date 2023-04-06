use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::task::{Context, Poll};

use either::Either;
use libp2p::PeerId;
use void::Void;

use algebra_core::CommutativePartialSemigroup;
use spectrum_crypto::VerifiableAgainst;

use crate::protocol_handler::handel::message::HandelMessage;
use crate::protocol_handler::{ProtocolBehaviourOut, TemporalProtocolStage};

mod message;

pub trait Weighted {
    fn weight(&self) -> usize;
}

pub struct Threshold {
    pub num: usize,
    pub denom: usize,
}

impl Threshold {
    pub fn min(&self, n: usize) -> usize {
        n * self.num / self.denom
    }
}

struct ActiveLevel<C> {
    prioritized_contributions: Vec<PendingContribution<C>>,
    best_contribution: Option<Verified<ScoredContribution<C>>>,
    individual_contributions: Vec<Verified<C>>,
}

pub struct HandelConfig {
    threshold: Threshold,
    window_shrinking_factor: usize,
    initial_scoring_window: usize,
}

/// A round of Handel protocol that drives aggregation of contribution `C`.
pub struct Handel<C, P> {
    conf: HandelConfig,
    public_data: P,
    scoring_window: usize,
    unverified_contributions: Vec<HashMap<PeerId, PendingContribution<C>>>,
    /// Peers partitioned by levels and ordered by Verification Priority within a level `l`.
    peer_partitions: Vec<Vec<PeerId>>,
    levels: Vec<Option<ActiveLevel<C>>>,
    byzantine_nodes: HashSet<PeerId>,
}

impl<C, P> Handel<C, P>
where
    C: CommutativePartialSemigroup + Weighted + VerifiableAgainst<P> + Eq + Clone,
{
    /// Run aggregation on the specified level.
    pub fn run(&mut self, level: usize) {
        if let Some(lvl) = &mut self.levels[level] {
            // Prioritize contributions
            if !self.unverified_contributions[level].is_empty() {
                for pid in &self.peer_partitions[level] {
                    if let Some(uc) = self.unverified_contributions[level].remove(pid) {
                        lvl.prioritized_contributions.push(uc);
                    }
                    if lvl.prioritized_contributions.len() >= self.scoring_window {
                        break;
                    }
                }
            }
            let best_contribution = lvl.best_contribution.clone().map(|Verified(vc)| vc.contribution);
            let mut scored_contributions: BTreeSet<ScoredContribution<C>> = BTreeSet::new();
            while let Some(c) = lvl.prioritized_contributions.pop() {
                // Verify individual contribution first
                if c.individual_contribution.verify(&self.public_data) {
                    lvl.individual_contributions
                        .push(Verified(c.individual_contribution))
                } else {
                    // Ban peer, skip scoring and verification of aggregate contribution from this peer.
                    self.byzantine_nodes.insert(c.sender_id);
                    continue;
                }
                // Score aggregate contribution
                match best_contribution
                    .as_ref()
                    .map(|bc| bc.try_combine(&c.aggregate_contribution))
                {
                    Some(Some(aggr)) => {
                        let score = aggr.weight();
                        scored_contributions.insert(ScoredContribution {
                            score,
                            sender_id: c.sender_id,
                            contribution: aggr,
                        });
                    }
                    Some(_) | None => {
                        let mut acc_aggr = c.aggregate_contribution.clone();
                        for Verified(ic) in &lvl.individual_contributions {
                            if let Some(aggr) = acc_aggr.try_combine(&ic) {
                                acc_aggr = aggr;
                            }
                        }
                        let score = acc_aggr.weight();
                        scored_contributions.insert(ScoredContribution {
                            score,
                            sender_id: c.sender_id,
                            contribution: acc_aggr,
                        });
                    }
                }
            }
            // Verify aggregate contributions
            for sc in scored_contributions.into_iter() {
                if sc.contribution.verify(&self.public_data) {
                    if let Some(Verified(best_contrib)) = &lvl.best_contribution {
                        if sc.score > best_contrib.score {
                            lvl.best_contribution = Some(Verified(sc))
                        }
                    } else {
                        lvl.best_contribution = Some(Verified(sc))
                    }
                } else {
                    self.byzantine_nodes.insert(sc.sender_id);
                    self.scoring_window = self.scoring_window / self.conf.window_shrinking_factor
                }
            }
        }
    }

    fn is_complete(&self, contribution: &C, level: usize) -> bool {
        let weight = contribution.weight();
        let num_nodes_at_level = (2 as usize).pow(level as u32);
        let threshold = self.conf.threshold.min(num_nodes_at_level);
        weight >= threshold
    }
}

struct PendingContribution<C> {
    sender_id: PeerId,
    aggregate_contribution: C,
    individual_contribution: C,
}

#[derive(Eq, PartialEq, Clone)]
struct ScoredContribution<C> {
    score: usize,
    sender_id: PeerId,
    contribution: C,
}

impl<C: PartialEq> PartialOrd for ScoredContribution<C> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.score.partial_cmp(&other.score)
    }
}

impl<C: Eq> Ord for ScoredContribution<C> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score.cmp(&other.score)
    }
}

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone)]
struct Verified<C>(C);

impl<C, P> TemporalProtocolStage<Void, HandelMessage<C>, C> for Handel<C, P>
where
    C: CommutativePartialSemigroup + Weighted + VerifiableAgainst<P> + Clone,
{
    fn inject_message(&mut self, peer_id: PeerId, HandelMessage::HandelMessageV1(msg): HandelMessage<C>) {
        if !self.byzantine_nodes.contains(&peer_id) {
            self.unverified_contributions[msg.level as usize].insert(
                peer_id,
                PendingContribution {
                    sender_id: peer_id,
                    aggregate_contribution: msg.aggregate_contribution,
                    individual_contribution: msg.individual_contribution,
                },
            );
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Either<ProtocolBehaviourOut<Void, HandelMessage<C>>, C>> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::protocol_handler::handel::Weighted;

    struct Contrib(HashSet<u32>);

    impl Weighted for Contrib {
        fn weight(&self) -> usize {
            self.0.len()
        }
    }
}
