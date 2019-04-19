// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::{
    state::{
        MemberState, ProcessElderChangeState, StartRelocatedNodeConnectionState,
        StartResourceProofState,
    },
    utilities::{
        Candidate, CandidateInfo, ChangeElder, LocalEvent, MergeInfo, Name, Node, ParsecVote,
        Proof, RelocatedInfo, Rpc, WaitedEvent,
    },
};
use unwrap::unwrap;

#[derive(Debug, PartialEq)]
pub struct RespondToRelocateRequests<'a>(pub &'a mut MemberState);

impl<'a> RespondToRelocateRequests<'a> {
    pub fn try_next(&mut self, event: WaitedEvent) -> Option<()> {
        match event {
            WaitedEvent::Rpc(rpc) => self.try_rpc(rpc),
            WaitedEvent::ParsecConsensus(vote) => self.try_consensus(vote),
            _ => None,
        }
    }

    fn try_rpc(&mut self, rpc: Rpc) -> Option<()> {
        match rpc {
            Rpc::ExpectCandidate(candidate) => Some(self.vote_parsec_expect_candidate(candidate)),
            _ => None,
        }
    }

    fn try_consensus(&mut self, vote: ParsecVote) -> Option<()> {
        match vote {
            ParsecVote::ExpectCandidate(candidate) => {
                Some(self.try_consensused_expect_candidate(candidate))
            }

            // Delegate to other event loops
            _ => None,
        }
    }

    fn try_consensused_expect_candidate(&mut self, candidate: Candidate) {
        match (
            self.0.action.get_waiting_candidate_info(candidate),
            self.0.action.count_waiting_proofing_or_hop(),
        ) {
            (Some(info), _) => self.resend_relocate_response_rpc(info),
            (_, 0) => self.add_node_and_send_relocate_response_rpc(candidate),
            (_, _) => self.send_refuse_candidate_rpc(candidate),
        }
    }

    fn add_node_and_send_relocate_response_rpc(&mut self, candidate: Candidate) {
        let relocated_info = self.0.action.add_node_waiting_candidate_info(candidate);
        self.0.action.send_relocate_response_rpc(relocated_info);
    }

    fn resend_relocate_response_rpc(&mut self, relocated_info: RelocatedInfo) {
        self.0.action.send_relocate_response_rpc(relocated_info);
    }

    fn send_refuse_candidate_rpc(&mut self, candidate: Candidate) {
        self.0.action.send_rpc(Rpc::RefuseCandidate(candidate));
    }

    fn vote_parsec_expect_candidate(&mut self, candidate: Candidate) {
        self.0
            .action
            .vote_parsec(ParsecVote::ExpectCandidate(candidate));
    }
}

#[derive(Debug, PartialEq)]
pub struct StartRelocatedNodeConnection<'a>(pub &'a mut MemberState);

impl<'a> StartRelocatedNodeConnection<'a> {
    // TODO - remove the `allow` once we have a test for this method.
    #[allow(dead_code)]
    fn start_event_loop(&mut self) {
        self.schedule_time_out()
    }

    pub fn try_next(&mut self, event: WaitedEvent) -> Option<()> {
        match event {
            WaitedEvent::Rpc(rpc) => self.try_rpc(rpc),
            WaitedEvent::ParsecConsensus(vote) => self.try_consensus(vote),
            WaitedEvent::LocalEvent(local_event) => self.try_local_event(local_event),
        }
    }

    fn try_rpc(&mut self, rpc: Rpc) -> Option<()> {
        match rpc {
            Rpc::CandidateInfo(info) => Some(self.rpc_info(info)),
            Rpc::ConnectionInfoResponse { .. } => {
                self.try_connect_and_vote_parsec_candidate_connected(rpc)
            }
            _ => None,
        }
    }

    fn try_consensus(&mut self, vote: ParsecVote) -> Option<()> {
        match vote {
            ParsecVote::CandidateConnected(info) => Some(self.check_candidate_connected(info)),
            ParsecVote::CheckRelocatedNodeConnection => Some({
                self.reject_candidates_that_took_too_long();
                self.schedule_time_out()
            }),
            // Delegate to other event loops
            _ => None,
        }
    }

    fn try_local_event(&mut self, local_event: LocalEvent) -> Option<()> {
        match local_event {
            LocalEvent::CheckRelocatedNodeConnectionTimeout => {
                Some(self.vote_parsec_check_relocated_node_connection())
            }
            _ => None,
        }
    }

    fn try_connect_and_vote_parsec_candidate_connected(&mut self, rpc: Rpc) -> Option<()> {
        if let Rpc::ConnectionInfoResponse { source, .. } = rpc {
            if !self.routine_state().candidates_voted.contains(&source) {
                if let Some(info) = self.routine_state().candidates_info.get(&source) {
                    self.0
                        .action
                        .vote_parsec(ParsecVote::CandidateConnected(*info));
                    let _ = self.mut_routine_state().candidates_voted.insert(source);

                    return Some(());
                }
            }
        }

        None
    }

    fn rpc_info(&mut self, info: CandidateInfo) {
        if self.0.action.is_valid_waited_info(info) {
            self.cache_candidate_info_and_send_connect_info(info)
        } else {
            self.discard()
        }
    }

    fn check_candidate_connected(&mut self, info: CandidateInfo) {
        if self.0.action.is_valid_waited_info(info) {
            self.check_update_to_node(info);
            self.send_node_connected_rpc(info)
        } else {
            self.discard()
        }
    }

    fn check_update_to_node(&mut self, info: CandidateInfo) {
        match self.0.action.check_shortest_prefix() {
            None => self.0.action.update_to_node_with_waiting_proof_state(info),
            Some(_) => self.0.action.update_to_node_with_relocating_hop_state(info),
        }
    }

    fn routine_state(&self) -> &StartRelocatedNodeConnectionState {
        &self.0.start_relocated_node_connection_state
    }

    fn mut_routine_state(&mut self) -> &mut StartRelocatedNodeConnectionState {
        &mut self.0.start_relocated_node_connection_state
    }

    fn discard(&mut self) {}

    fn reject_candidates_that_took_too_long(&mut self) {
        let new_connecting_nodes = self.0.action.waiting_nodes_connecting();
        let nodes_to_remove: Vec<Name> = new_connecting_nodes
            .intersection(&self.routine_state().candidates)
            .cloned()
            .collect();

        for name in nodes_to_remove {
            self.0.action.purge_node_info(name);
        }

        let candidates = self.0.action.waiting_nodes_connecting();
        let mut_routine_state = &mut self.mut_routine_state();

        mut_routine_state.candidates = candidates.clone();
        mut_routine_state.candidates_info = mut_routine_state
            .candidates_info
            .clone()
            .into_iter()
            .filter(|(name, _)| candidates.contains(name))
            .collect();
        mut_routine_state.candidates_voted = mut_routine_state
            .candidates_voted
            .clone()
            .into_iter()
            .filter(|name| candidates.contains(name))
            .collect();
    }

    fn cache_candidate_info_and_send_connect_info(&mut self, info: CandidateInfo) {
        let _ = self
            .mut_routine_state()
            .candidates_info
            .insert(info.new_public_id.name(), info);
        self.0
            .action
            .send_connection_info_request(info.new_public_id.name());
    }

    fn schedule_time_out(&mut self) {
        self.0
            .action
            .schedule_event(LocalEvent::CheckRelocatedNodeConnectionTimeout);
    }

    fn send_node_connected_rpc(&mut self, info: CandidateInfo) {
        self.0.action.send_node_connected(info.new_public_id);
    }

    fn vote_parsec_check_relocated_node_connection(&mut self) {
        self.0
            .action
            .vote_parsec(ParsecVote::CheckRelocatedNodeConnection);
    }
}

#[derive(Debug, PartialEq)]
pub struct StartResourceProof<'a>(pub &'a mut MemberState);

impl<'a> StartResourceProof<'a> {
    // TODO - remove the `allow` once we have a test for this method.
    #[allow(dead_code)]
    fn start_event_loop(&mut self) {
        self.0
            .action
            .schedule_event(LocalEvent::CheckResourceProofTimeout);
    }

    pub fn try_next(&mut self, event: WaitedEvent) -> Option<()> {
        match event {
            WaitedEvent::Rpc(Rpc::ResourceProofResponse {
                candidate, proof, ..
            }) => Some(self.rpc_proof(candidate, proof)),
            WaitedEvent::ParsecConsensus(vote) => self.try_consensus(vote),
            WaitedEvent::LocalEvent(local_event) => self.try_local_event(local_event),
            // Delegate to other event loops
            _ => None,
        }
    }

    fn rpc_proof(&mut self, candidate: Candidate, proof: Proof) {
        let from_candidate = self.has_candidate() && candidate == self.candidate();

        if from_candidate && !self.routine_state().voted_online && proof.is_valid() {
            match proof {
                Proof::ValidPart => self.send_resource_proof_receipt_rpc(),
                Proof::ValidEnd => {
                    self.set_voted_online(true);
                    self.vote_parsec_online_candidate();
                }
                Proof::Invalid => panic!("Only valid proof"),
            }
        } else {
            self.discard()
        }
    }

    fn try_consensus(&mut self, vote: ParsecVote) -> Option<()> {
        let for_candidate = self.has_candidate() && vote.candidate() == Some(self.candidate());

        match vote {
            ParsecVote::CheckResourceProof => Some({
                self.set_resource_proof_candidate();
                self.check_request_resource_proof();
            }),
            ParsecVote::Online(_) if for_candidate => Some(self.make_node_online()),
            ParsecVote::PurgeCandidate(_) if for_candidate => Some(self.purge_node_info()),
            ParsecVote::Online(_) | ParsecVote::PurgeCandidate(_) => Some(self.discard()),

            // Delegate to other event loops
            _ => None,
        }
    }

    fn try_local_event(&mut self, local_event: LocalEvent) -> Option<()> {
        match local_event {
            LocalEvent::TimeoutAccept => Some(self.vote_parsec_purge_candidate()),
            LocalEvent::CheckResourceProofTimeout => Some(self.vote_parsec_check_resource_proof()),
            _ => None,
        }
    }

    fn routine_state(&self) -> &StartResourceProofState {
        &self.0.start_resource_proof
    }

    fn mut_routine_state(&mut self) -> &mut StartResourceProofState {
        &mut self.0.start_resource_proof
    }

    fn discard(&mut self) {}

    fn set_resource_proof_candidate(&mut self) {
        self.mut_routine_state().candidate = self.0.action.resource_proof_candidate();
    }

    // TODO - remove the `allow` once we have a test for this method.
    #[allow(dead_code)]
    fn set_got_candidate_info(&mut self, value: bool) {
        self.mut_routine_state().got_candidate_info = value;
    }

    fn set_voted_online(&mut self, value: bool) {
        self.mut_routine_state().voted_online = value;
    }

    fn vote_parsec_purge_candidate(&mut self) {
        self.0
            .action
            .vote_parsec(ParsecVote::PurgeCandidate(self.candidate()));
    }

    fn vote_parsec_check_resource_proof(&mut self) {
        self.0.action.vote_parsec(ParsecVote::CheckResourceProof);
    }

    fn vote_parsec_online_candidate(&mut self) {
        self.0
            .action
            .vote_parsec(ParsecVote::Online(self.candidate()));
    }

    fn make_node_online(&mut self) {
        self.0.action.set_candidate_online_state(self.candidate());
        self.0.action.send_node_approval_rpc(self.candidate());
        self.finish_resource_proof()
    }

    fn purge_node_info(&mut self) {
        self.0.action.purge_node_info(self.candidate().name());
        self.finish_resource_proof()
    }

    fn finish_resource_proof(&mut self) {
        self.mut_routine_state().candidate = None;
        self.mut_routine_state().voted_online = false;
        self.mut_routine_state().got_candidate_info = false;

        self.0
            .action
            .schedule_event(LocalEvent::CheckResourceProofTimeout);
    }

    fn check_request_resource_proof(&mut self) {
        if self.has_candidate() {
            self.send_resource_proof_rpc()
        } else {
            self.finish_resource_proof()
        }
    }

    fn send_resource_proof_rpc(&mut self) {
        self.0.action.send_candidate_proof_request(self.candidate());
    }

    fn send_resource_proof_receipt_rpc(&mut self) {
        self.0.action.send_candidate_proof_receipt(self.candidate());
    }

    fn candidate(&self) -> Candidate {
        unwrap!(self.routine_state().candidate)
    }

    fn has_candidate(&self) -> bool {
        self.routine_state().candidate.is_some()
    }
}

#[derive(Debug, PartialEq)]
pub struct CheckAndProcessElderChange<'a>(pub &'a mut MemberState);

// CheckAndProcessElderChange Sub Routine
impl<'a> CheckAndProcessElderChange<'a> {
    // TODO - remove the `allow` once we have a test for this method.
    #[allow(dead_code)]
    fn start_event_loop(&mut self) {
        self.start_check_elder_timeout()
    }

    pub fn try_next(&mut self, event: WaitedEvent) -> Option<()> {
        match event {
            WaitedEvent::ParsecConsensus(vote) => self.try_consensus(&vote),
            WaitedEvent::Rpc(rpc) => self.try_rpc(rpc),
            WaitedEvent::LocalEvent(LocalEvent::TimeoutCheckElder) => {
                Some(self.vote_parsec_check_elder())
            }
            _ => None,
        }
    }

    fn try_consensus(&mut self, vote: &ParsecVote) -> Option<()> {
        match vote {
            ParsecVote::NeighbourMerge(merge_info) => Some(self.store_merge_infos(*merge_info)),
            ParsecVote::CheckElder => Some(self.check_merge()),
            _ => None,
        }
    }

    fn try_rpc(&mut self, rpc: Rpc) -> Option<()> {
        match rpc {
            Rpc::Merge => Some(self.vote_parsec_neighbour_merge()),
            _ => None,
        }
    }

    fn store_merge_infos(&mut self, merge_info: MergeInfo) {
        self.0.action.store_merge_infos(merge_info);
    }

    fn merge_needed(&mut self) -> bool {
        self.0.action.merge_needed()
    }

    fn has_merge_infos(&mut self) -> bool {
        self.0.action.has_merge_infos()
    }

    fn check_merge(&mut self) {
        if self.has_merge_infos() || self.merge_needed() {
            // TODO: -> Concurrent to ProcessMerge
            self.0.action.send_rpc(Rpc::Merge);
        } else {
            self.check_elder()
        }
    }

    fn check_elder(&mut self) {
        match self.0.action.check_elder() {
            Some(change_elder) => self.concurrent_transition_to_process_elder_change(change_elder),
            None => self.start_check_elder_timeout(),
        }
    }

    fn concurrent_transition_to_process_elder_change(&mut self, change_elder: ChangeElder) {
        self.0
            .as_process_elder_change()
            .start_event_loop(change_elder)
    }

    fn transition_exit_process_elder_change(&mut self) {
        self.start_check_elder_timeout()
    }

    fn vote_parsec_check_elder(&mut self) {
        self.0.action.vote_parsec(ParsecVote::CheckElder);
    }

    fn vote_parsec_neighbour_merge(&mut self) {
        self.0
            .action
            .vote_parsec(ParsecVote::NeighbourMerge(MergeInfo));
    }

    fn start_check_elder_timeout(&mut self) {
        self.0.action.schedule_event(LocalEvent::TimeoutCheckElder);
    }
}

#[derive(Debug, PartialEq)]
pub struct ProcessElderChange<'a>(pub &'a mut MemberState);

impl<'a> ProcessElderChange<'a> {
    pub fn start_event_loop(&mut self, change_elder: ChangeElder) {
        self.mut_routine_state().is_active = true;
        self.mut_routine_state().change_elder = Some(change_elder.clone());
        self.vote_for_elder_change(change_elder)
    }

    fn exit_event_loop(&mut self) {
        self.mut_routine_state().is_active = false;
        self.mut_routine_state().change_elder = None;
        self.0
            .as_check_and_process_elder_change()
            .transition_exit_process_elder_change()
    }

    pub fn try_next(&mut self, event: WaitedEvent) -> Option<()> {
        match event {
            WaitedEvent::ParsecConsensus(vote) => self.try_consensus(&vote),
            _ => None,
        }
    }

    fn try_consensus(&mut self, vote: &ParsecVote) -> Option<()> {
        if !self.routine_state().wait_votes.contains(&vote) {
            return None;
        }

        let wait_votes = &mut self.mut_routine_state().wait_votes;
        wait_votes.retain(|wait_vote| wait_vote != vote);

        if wait_votes.is_empty() {
            Some({
                self.mark_elder_change();
                self.exit_event_loop()
            })
        } else {
            Some(())
        }
    }

    fn vote_for_elder_change(&mut self, change_elder: ChangeElder) {
        let votes = self.0.action.get_elder_change_votes(&change_elder);
        self.mut_routine_state().change_elder = Some(change_elder);
        self.mut_routine_state().wait_votes = votes;

        for vote in &self.routine_state().wait_votes {
            self.0.action.vote_parsec(*vote);
        }
    }

    fn routine_state(&self) -> &ProcessElderChangeState {
        &self
            .0
            .check_and_process_elder_change_routine
            .sub_routine_process_elder_change
    }

    fn mut_routine_state(&mut self) -> &mut ProcessElderChangeState {
        &mut self
            .0
            .check_and_process_elder_change_routine
            .sub_routine_process_elder_change
    }

    fn mark_elder_change(&mut self) {
        let change_elder = unwrap!(self.mut_routine_state().change_elder.take());
        self.0.action.mark_elder_change(change_elder);
    }
}

#[derive(Debug, PartialEq)]
pub struct CheckOnlineOffline<'a>(pub &'a mut MemberState);

impl<'a> CheckOnlineOffline<'a> {
    pub fn try_next(&mut self, event: WaitedEvent) -> Option<()> {
        match event {
            WaitedEvent::ParsecConsensus(vote) => self.try_consensus(&vote),
            WaitedEvent::LocalEvent(local_event) => self.try_local_event(local_event),
            // Delegate to other event loops
            _ => None,
        }
    }

    fn try_consensus(&mut self, vote: &ParsecVote) -> Option<()> {
        match vote {
            ParsecVote::Offline(node) => Some(self.make_node_offline(*node)),
            ParsecVote::BackOnline(node) => Some(self.make_node_back_online(*node)),
            // Delegate to other event loops
            _ => None,
        }
    }

    fn try_local_event(&mut self, local_event: LocalEvent) -> Option<()> {
        match local_event {
            LocalEvent::NodeDetectedOffline(node) => Some(self.vote_parsec_offline(node)),
            LocalEvent::NodeDetectedBackOnline(node) => Some(self.vote_parsec_back_online(node)),
            // Delegate to other event loops
            _ => None,
        }
    }

    fn vote_parsec_offline(&mut self, node: Node) {
        self.0.action.vote_parsec(ParsecVote::Offline(node));
    }

    fn vote_parsec_back_online(&mut self, node: Node) {
        self.0.action.vote_parsec(ParsecVote::BackOnline(node));
    }

    fn make_node_offline(&mut self, node: Node) {
        self.0.action.set_node_offline_state(node);
    }

    /// A member of a section that was lost connection to became offline, but is now online again
    fn make_node_back_online(&mut self, node: Node) {
        self.0.action.set_node_back_online_state(node);
    }
}
