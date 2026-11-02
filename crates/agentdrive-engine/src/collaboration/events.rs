use super::*;

impl CollaborationService {
    pub(super) async fn accept_event(
        &self,
        actor: &Principal,
        request: CollaborationEvent,
    ) -> Result<CollaborationStatus> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        for value in [&request.source, &request.thread_id, &request.turn_id] {
            if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                return Err(Error::invalid(
                    "event identifiers must contain 1 to 256 printable characters",
                ));
            }
        }
        let key = object_ref(
            ObjectKind::RecordIndex,
            &encode(&(
                request.mount,
                &request.source,
                &request.thread_id,
                &request.turn_id,
            ))?,
        )
        .id;
        let request_id = format!("collaboration-execution-{key}");
        let previous = self
            .runtime
            .state
            .operation_by_request(actor, &request_id)
            .await?;
        let current_binding = self.target(actor, request.mount, Permission::Write).await?;
        let matches_execution = |execution: &CollaborationExecution| {
            execution.actor == *actor
                && execution.mount == request.mount
                && execution.source == request.source
                && execution.thread_id == request.thread_id
                && execution.turn_id == request.turn_id
        };
        let continued = self
            .runtime
            .state
            .collaboration(current_binding.branch)
            .await?
            .is_some_and(|record| record.executions.iter().any(&matches_execution));
        let branch_id = if continued {
            current_binding.branch
        } else {
            previous
                .as_ref()
                .and_then(|operation| operation.branch)
                .unwrap_or(current_binding.branch)
        };
        let _coordinate = self.coordinate(branch_id)?;
        let mut record = self
            .runtime
            .state
            .collaboration(branch_id)
            .await?
            .ok_or_else(|| {
                Error::invalid("automatic collaboration is not configured for this execution")
            })?;
        self.runtime
            .authorize(
                actor,
                record.workspace,
                Some(record.branch),
                Permission::Write,
            )
            .await?;
        let branch = self.runtime.branch(record.branch).await?;
        let position = record.executions.iter().position(matches_execution);
        if request.phase == ExecutionPhase::Started {
            if position.is_some()
                || previous
                    .as_ref()
                    .is_some_and(|operation| operation.phase == OperationPhase::Complete)
            {
                return self.describe(&record).await;
            }
            if !record.settings.enabled || !self.automatic_enabled() {
                return Err(Error::invalid("automatic collaboration is disabled"));
            }
            if record.executions.len() >= 1024 {
                return Err(Error::new(
                    ErrorCode::CapacityExceeded,
                    "too many active executions for this branch",
                ));
            }
            if record.transition.is_some() || record.recovery.is_some() {
                return Err(Error::new(
                    ErrorCode::BranchBusy,
                    "branch collaboration is completing a transition",
                )
                .retryable());
            }
            let start = self
                .operations
                .start(
                    RequestContext {
                        principal: actor.clone(),
                        request_id,
                    },
                    "collaboration_execution",
                    &(
                        request.mount,
                        &request.source,
                        &request.thread_id,
                        &request.turn_id,
                    ),
                    Some(record.workspace),
                    Some(record.branch),
                )
                .await?;
            let mut operation = match start {
                OperationStart::New(context) => context.operation,
                OperationStart::Existing(operation) => operation,
            };
            operation.phase = OperationPhase::Complete;
            operation.local_saved = true;
            operation.result = Some(CommandResult::Updated);
            record.executions.push(CollaborationExecution {
                actor: actor.clone(),
                mount: request.mount,
                binding_generation: current_binding.generation,
                source: request.source,
                thread_id: request.thread_id,
                turn_id: request.turn_id,
                authority_epoch: branch.authority_epoch,
                pending_grant: !(branch.owner == self.runtime.location
                    && branch.state == BranchState::Writable),
                phase: ExecutionPhase::Started,
            });
            record.release = None;
            if record.phase == CollaborationPhase::WaitingForHandles {
                record.phase = CollaborationPhase::Writing;
            }
            self.runtime
                .state
                .commit(LocalCommit {
                    operations: vec![operation],
                    collaborations: vec![record.clone()],
                    ..Default::default()
                })
                .await?;
        } else {
            if previous.is_none() {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    "execution must be registered by this principal before its completion event",
                ));
            }
            let Some(position) = position else {
                if previous.is_some() {
                    return self.describe(&record).await;
                }
                return Err(Error::new(
                    ErrorCode::NotFound,
                    "execution must be registered before its completion event",
                ));
            };
            let execution = record.executions.remove(position);
            let current_grant = !execution.pending_grant
                && execution.authority_epoch == branch.authority_epoch
                && current_binding.branch == record.branch
                && current_binding.generation == execution.binding_generation;
            let still_active = record.executions.iter().any(|execution| {
                !execution.pending_grant && execution.authority_epoch == branch.authority_epoch
            });
            if record.settings.enabled
                && self.automatic_enabled()
                && record.settings.release_on_event
                && current_grant
                && !still_active
                && branch.owner == self.runtime.location
                && branch.state == BranchState::Writable
            {
                record.release = Some(CollaborationRelease {
                    operation: OperationId::new(),
                    authority_epoch: branch.authority_epoch,
                    mutation_seq: branch.mutation_seq,
                    reason: ReleaseReason::ExecutionEnded,
                });
            }
            self.save_record(&record).await?;
        }
        self.describe(&record).await
    }
}
