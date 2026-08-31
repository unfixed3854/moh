//! Terminal workspace ownership over one backend connection and at most one attachment.

use std::{future, time::Duration};

use moh::{
    rpc::client::{
        MaterializedSession, RpcBackendClient, RpcSessionClient, RpcStartup, SessionUpdate,
    },
    runtime::rig::ReasoningLevel,
    session::{
        ActiveRunSnapshot, JobSnapshotDto, ModelCatalogState, SessionEvent, SessionEventEnvelope,
        SessionId, SessionListScope, SessionSelector, SessionSettings, SessionSnapshot,
        SessionTitle, TranscriptItem,
    },
    tools::JobState,
};

use super::session::{
    ChatProjection, ClientSessionError, DraftState, LaunchMode, SessionListFuture, WorkspaceClient,
    WorkspaceUpdate,
};

const BACKEND_READY_INTERVAL: Duration = Duration::from_millis(25);

pub(crate) enum WorkspaceStartup<S> {
    Draft(DraftState),
    Attached(S),
}

#[allow(dead_code)] // Constructed through submit once Task 12 drives draft presentation.
pub(crate) struct WorkspaceMaterialized<S> {
    pub(crate) session: S,
    pub(crate) run_id: u64,
}

#[allow(dead_code)] // The full command surface is consumed by Tasks 12-15.
pub(crate) trait WorkspaceSession: Sized {
    fn snapshot(&self) -> &SessionSnapshot;
    async fn next_update(&mut self) -> Result<SessionUpdate, ClientSessionError>;
    async fn submit(&self, prompt: String) -> Result<u64, ClientSessionError>;
    async fn cancel(&self) -> Result<(), ClientSessionError>;
    async fn select_model(&self, model: String) -> Result<(), ClientSessionError>;
    async fn select_reasoning(&self, reasoning: ReasoningLevel) -> Result<(), ClientSessionError>;
    async fn list_jobs(&self) -> Result<Vec<JobSnapshotDto>, ClientSessionError>;
    async fn cancel_job(&self, id: String) -> Result<JobSnapshotDto, ClientSessionError>;
    async fn detach(self) -> Result<(), ClientSessionError>;
}

#[allow(dead_code)] // The full command surface is consumed by Tasks 12-15.
pub(crate) trait WorkspaceBackend: Sized {
    type Session: WorkspaceSession;

    async fn draft_defaults(&self, cwd: Vec<u8>) -> Result<DraftState, ClientSessionError>;
    async fn startup(
        &self,
        cwd: Vec<u8>,
    ) -> Result<WorkspaceStartup<Self::Session>, ClientSessionError>;
    async fn materialize(
        &self,
        cwd: Vec<u8>,
        prompt: String,
        settings: SessionSettings,
    ) -> Result<WorkspaceMaterialized<Self::Session>, ClientSessionError>;
    async fn open_session(
        &self,
        selector: SessionSelector,
        cwd_for_title: Vec<u8>,
    ) -> Result<Self::Session, ClientSessionError>;
    fn list_sessions(&self, scope: SessionListScope) -> SessionListFuture;
    async fn rename_session(
        &self,
        session_id: SessionId,
        title: SessionTitle,
    ) -> Result<(), ClientSessionError>;
    async fn delete_session(&self, session_id: SessionId) -> Result<(), ClientSessionError>;
    async fn disconnect(self) -> Result<(), ClientSessionError>;
}

pub(crate) struct WorkspaceController<B: WorkspaceBackend> {
    backend: B,
    session: Option<B::Session>,
    projection: ChatProjection,
    pending_warning: Option<String>,
}

pub(crate) type RpcWorkspaceController = WorkspaceController<RpcBackendClient>;

#[allow(dead_code)] // Helpers are reached through the Task 12 WorkspaceClient event loop.
impl<B: WorkspaceBackend> WorkspaceController<B> {
    pub(crate) async fn launch(
        backend: B,
        cwd: Vec<u8>,
        mode: LaunchMode,
    ) -> Result<Self, ClientSessionError> {
        match mode {
            LaunchMode::NewDraft => {
                let draft = loop {
                    match backend.draft_defaults(cwd.clone()).await {
                        Err(error) if error.is_backend_starting() => {
                            tokio::time::sleep(BACKEND_READY_INTERVAL).await;
                        }
                        result => break result?,
                    }
                };
                Ok(Self {
                    backend,
                    session: None,
                    projection: ChatProjection::Draft(draft),
                    pending_warning: None,
                })
            }
            LaunchMode::Startup => {
                let startup = loop {
                    match backend.startup(cwd.clone()).await {
                        Err(error) if error.is_backend_starting() => {
                            tokio::time::sleep(BACKEND_READY_INTERVAL).await;
                        }
                        result => break result?,
                    }
                };
                Ok(Self::from_startup(backend, startup))
            }
            LaunchMode::Session(selector) => {
                let session = loop {
                    match backend.open_session(selector.clone(), cwd.clone()).await {
                        Err(error) if error.is_backend_starting() => {
                            tokio::time::sleep(BACKEND_READY_INTERVAL).await;
                        }
                        result => break result?,
                    }
                };
                Ok(Self::from_session(backend, session))
            }
        }
    }

    fn from_startup(backend: B, startup: WorkspaceStartup<B::Session>) -> Self {
        match startup {
            WorkspaceStartup::Draft(draft) => Self {
                backend,
                session: None,
                projection: ChatProjection::Draft(draft),
                pending_warning: None,
            },
            WorkspaceStartup::Attached(session) => Self::from_session(backend, session),
        }
    }

    fn from_session(backend: B, session: B::Session) -> Self {
        let projection = ChatProjection::session(session.snapshot().clone());
        Self {
            backend,
            session: Some(session),
            projection,
            pending_warning: None,
        }
    }

    pub(crate) fn attached_mut(&mut self) -> Option<&mut B::Session> {
        self.session.as_mut()
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), ClientSessionError> {
        let detach = match self.session.take() {
            Some(session) => session.detach().await,
            None => Ok(()),
        };
        let disconnect = self.backend.disconnect().await;
        detach.and(disconnect)
    }

    fn draft_from_current(&self) -> DraftState {
        let cwd = match &self.projection {
            ChatProjection::Draft(draft) => draft.cwd.clone(),
            ChatProjection::Session(snapshot) => snapshot.summary.cwd.clone(),
        };
        self.draft_for_cwd(cwd)
    }

    fn draft_for_cwd(&self, cwd: Vec<u8>) -> DraftState {
        let (mut settings, catalog) = match &self.projection {
            ChatProjection::Draft(draft) => (draft.settings.clone(), draft.catalog.clone()),
            ChatProjection::Session(snapshot) => {
                (snapshot.settings.clone(), snapshot.catalog.clone())
            }
        };
        settings.context_tokens = 0;
        DraftState {
            cwd,
            settings,
            catalog,
        }
    }

    fn install_session(&mut self, session: B::Session) -> Option<B::Session> {
        let snapshot = session.snapshot().clone();
        self.projection = ChatProjection::session(snapshot);
        self.session.replace(session)
    }

    fn install_startup(&mut self, startup: WorkspaceStartup<B::Session>) {
        match startup {
            WorkspaceStartup::Draft(draft) => {
                self.session = None;
                self.projection = ChatProjection::Draft(draft);
            }
            WorkspaceStartup::Attached(session) => {
                self.install_session(session);
            }
        }
    }

    fn attached(&self) -> Result<&B::Session, ClientSessionError> {
        self.session
            .as_ref()
            .ok_or_else(|| ClientSessionError::message("the current chat is not durable"))
    }

    fn retain_warning(&mut self, error: ClientSessionError) {
        if self.pending_warning.is_none() {
            self.pending_warning = Some(error.to_string());
        }
    }
}

impl<B: WorkspaceBackend> WorkspaceClient for WorkspaceController<B> {
    fn current_projection(&self) -> &ChatProjection {
        &self.projection
    }

    async fn next_update(&mut self) -> Result<WorkspaceUpdate, ClientSessionError> {
        if let Some(warning) = self.pending_warning.take() {
            return Ok(WorkspaceUpdate::Warning(warning));
        }
        let update = match self.session.as_mut() {
            Some(session) => session.next_update().await?,
            None => return future::pending().await,
        };
        match update {
            SessionUpdate::Warning(warning) => Ok(WorkspaceUpdate::Warning(warning)),
            SessionUpdate::Deleted { session_id, cwd } => {
                self.session = None;
                self.startup_fallback(cwd.clone()).await?;
                Ok(WorkspaceUpdate::Deleted { session_id, cwd })
            }
            update => {
                apply_projection_update(&mut self.projection, &update)?;
                Ok(WorkspaceUpdate::Session(update))
            }
        }
    }

    async fn submit(&mut self, prompt: &str) -> Result<u64, ClientSessionError> {
        match &self.projection {
            ChatProjection::Session(_) => self.attached()?.submit(prompt.to_owned()).await,
            ChatProjection::Draft(draft) => {
                let materialized = self
                    .backend
                    .materialize(draft.cwd.clone(), prompt.to_owned(), draft.settings.clone())
                    .await?;
                let run_id = materialized.run_id;
                self.install_session(materialized.session);
                Ok(run_id)
            }
        }
    }

    async fn cancel(&self) -> Result<(), ClientSessionError> {
        self.attached()?.cancel().await
    }

    async fn select_model(&mut self, model: String) -> Result<(), ClientSessionError> {
        match &mut self.projection {
            ChatProjection::Session(_) => self.attached()?.select_model(model).await,
            ChatProjection::Draft(draft) => {
                let ModelCatalogState::Ready(models) = &draft.catalog else {
                    return Err(ClientSessionError::message("model catalog is not ready"));
                };
                let selected = models
                    .iter()
                    .find(|candidate| candidate.id == model)
                    .ok_or_else(|| ClientSessionError::message("model is not available"))?;
                draft.settings.model = model;
                if !selected
                    .reasoning_efforts
                    .contains(&draft.settings.reasoning)
                {
                    draft.settings.reasoning = selected
                        .default_reasoning
                        .or_else(|| selected.reasoning_efforts.first().copied())
                        .ok_or_else(|| {
                            ClientSessionError::message("model has no supported reasoning effort")
                        })?;
                }
                draft.settings.context_tokens = 0;
                Ok(())
            }
        }
    }

    async fn select_reasoning(
        &mut self,
        reasoning: ReasoningLevel,
    ) -> Result<(), ClientSessionError> {
        match &mut self.projection {
            ChatProjection::Session(_) => self.attached()?.select_reasoning(reasoning).await,
            ChatProjection::Draft(draft) => {
                let ModelCatalogState::Ready(models) = &draft.catalog else {
                    return Err(ClientSessionError::message("model catalog is not ready"));
                };
                let supported = models
                    .iter()
                    .find(|model| model.id == draft.settings.model)
                    .is_some_and(|model| model.reasoning_efforts.contains(&reasoning));
                if !supported {
                    return Err(ClientSessionError::message(
                        "reasoning effort is not supported by the active model",
                    ));
                }
                draft.settings.reasoning = reasoning;
                draft.settings.context_tokens = 0;
                Ok(())
            }
        }
    }

    async fn list_jobs(&self) -> Result<Vec<JobSnapshotDto>, ClientSessionError> {
        match &self.projection {
            ChatProjection::Draft(_) => Ok(Vec::new()),
            ChatProjection::Session(_) => self.attached()?.list_jobs().await,
        }
    }

    async fn cancel_job(&self, id: String) -> Result<JobSnapshotDto, ClientSessionError> {
        self.attached()?.cancel_job(id).await
    }

    async fn new_draft(&mut self) -> Result<(), ClientSessionError> {
        let draft = self.draft_from_current();
        let old = self.session.take();
        self.projection = ChatProjection::Draft(draft);
        if let Some(session) = old
            && let Err(error) = session.detach().await
        {
            self.retain_warning(error);
        }
        Ok(())
    }

    fn list_sessions(&self, scope: SessionListScope) -> SessionListFuture {
        self.backend.list_sessions(scope)
    }

    async fn switch_session(&mut self, id: SessionId) -> Result<(), ClientSessionError> {
        let cwd = match &self.projection {
            ChatProjection::Draft(draft) => draft.cwd.clone(),
            ChatProjection::Session(snapshot) => snapshot.summary.cwd.clone(),
        };
        let target = self
            .backend
            .open_session(SessionSelector::Id(id), cwd)
            .await?;
        if target.snapshot().summary.id != id {
            let _ = target.detach().await;
            return Err(ClientSessionError::message(
                "backend opened an unexpected session",
            ));
        }
        let old = self.install_session(target);
        if let Some(session) = old
            && let Err(error) = session.detach().await
        {
            self.retain_warning(error);
        }
        Ok(())
    }

    async fn rename_session(
        &self,
        id: SessionId,
        title: SessionTitle,
    ) -> Result<(), ClientSessionError> {
        self.backend.rename_session(id, title).await
    }

    async fn delete_session(&mut self, id: SessionId) -> Result<(), ClientSessionError> {
        let current_cwd = match &self.projection {
            ChatProjection::Session(snapshot) if snapshot.summary.id == id => {
                Some(snapshot.summary.cwd.clone())
            }
            _ => None,
        };
        if let Err(delete_error) = self.backend.delete_session(id).await {
            if let Some(cwd) = current_cwd {
                match self
                    .backend
                    .open_session(SessionSelector::Id(id), cwd)
                    .await
                {
                    Ok(reopened) if reopened.snapshot().summary.id == id => {
                        let old = self.install_session(reopened);
                        if let Some(session) = old
                            && let Err(error) = session.detach().await
                        {
                            self.retain_warning(error);
                        }
                    }
                    Ok(unexpected) => {
                        let _ = unexpected.detach().await;
                        self.retain_warning(ClientSessionError::message(
                            "backend reopened an unexpected session after deletion failed",
                        ));
                    }
                    Err(error) => self.retain_warning(error),
                }
            }
            return Err(delete_error);
        }
        if let Some(cwd) = current_cwd {
            self.session = None;
            self.startup_fallback(cwd).await?;
        }
        Ok(())
    }

    async fn startup_fallback(&mut self, cwd: Vec<u8>) -> Result<(), ClientSessionError> {
        let draft = self.draft_for_cwd(cwd.clone());
        self.session = None;
        self.projection = ChatProjection::Draft(draft);
        match self.backend.startup(cwd).await {
            Ok(startup) => self.install_startup(startup),
            Err(error) => self.retain_warning(error),
        }
        Ok(())
    }
}

fn apply_projection_update(
    projection: &mut ChatProjection,
    update: &SessionUpdate,
) -> Result<(), ClientSessionError> {
    let ChatProjection::Session(current) = projection else {
        return Err(ClientSessionError::message(
            "received a session update while presenting a draft",
        ));
    };
    match update {
        SessionUpdate::SnapshotReplaced(snapshot) => {
            validate_snapshot(snapshot)?;
            current.clone_from(snapshot);
        }
        SessionUpdate::Event(envelope) => {
            let mut next = current.clone();
            apply_session_event(&mut next, envelope)?;
            *current = next;
        }
        SessionUpdate::Warning(_) => {}
        SessionUpdate::Deleted { .. } => {
            return Err(ClientSessionError::message(
                "deleted session update requires startup fallback",
            ));
        }
    }
    Ok(())
}

fn validate_snapshot(snapshot: &SessionSnapshot) -> Result<(), ClientSessionError> {
    if snapshot.busy != snapshot.summary.busy || snapshot.busy != snapshot.active_run.is_some() {
        return Err(ClientSessionError::message(
            "replacement snapshot has inconsistent busy state",
        ));
    }
    Ok(())
}

fn apply_session_event(
    projection: &mut SessionSnapshot,
    envelope: &SessionEventEnvelope,
) -> Result<(), ClientSessionError> {
    if projection.sequence.checked_add(1) != Some(envelope.sequence) {
        return Err(ClientSessionError::message(
            "session event sequence is not contiguous",
        ));
    }
    validate_event_run(projection, &envelope.event)?;
    match &envelope.event {
        SessionEvent::TitleChanged {
            title,
            title_revision,
        } => {
            projection.summary.title = title.clone();
            projection.summary.title_revision = *title_revision;
        }
        SessionEvent::Started { run_id, prompt } => {
            projection
                .transcript
                .push(TranscriptItem::User(prompt.clone()));
            projection.active_run = Some(ActiveRunSnapshot {
                run_id: *run_id,
                prompt: prompt.clone(),
                assistant_text: String::new(),
            });
            projection.busy = true;
            projection.summary.busy = true;
            refresh_running_summary(projection);
        }
        SessionEvent::AssistantDelta { text, .. } => projection
            .active_run
            .as_mut()
            .expect("validated active run")
            .assistant_text
            .push_str(text),
        SessionEvent::ContextUsage {
            input_tokens,
            last_activity,
            ..
        } => {
            projection.settings.context_tokens = *input_tokens;
            projection.summary.last_activity = *last_activity;
        }
        SessionEvent::ToolStarted {
            run_id,
            call_id,
            name,
            arguments,
        } => projection.transcript.push(TranscriptItem::ToolStarted {
            run_id: *run_id,
            call_id: call_id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
        }),
        SessionEvent::ToolFinished { .. } => {}
        SessionEvent::Completed {
            response,
            last_activity,
            ..
        } => {
            projection
                .transcript
                .push(TranscriptItem::Assistant(response.clone()));
            projection.active_run = None;
            projection.busy = false;
            projection.summary.busy = false;
            projection.summary.last_activity = *last_activity;
            refresh_running_summary(projection);
        }
        SessionEvent::Failed { run_id, failure } => {
            projection.transcript.push(TranscriptItem::Failed {
                run_id: *run_id,
                failure: failure.clone(),
            });
            projection.active_run = None;
            projection.busy = false;
            projection.summary.busy = false;
            refresh_running_summary(projection);
        }
        SessionEvent::Cancelled { run_id } => {
            projection
                .transcript
                .push(TranscriptItem::Cancelled { run_id: *run_id });
            projection.active_run = None;
            projection.busy = false;
            projection.summary.busy = false;
            refresh_running_summary(projection);
        }
        SessionEvent::SettingsChanged {
            settings,
            last_activity,
        } => {
            projection.settings = settings.clone();
            projection.summary.last_activity = *last_activity;
        }
        SessionEvent::JobsChanged(jobs) => {
            projection.jobs.clone_from(jobs);
            refresh_running_summary(projection);
        }
        SessionEvent::PlanChanged(plan) => projection.plan.clone_from(plan),
        SessionEvent::CatalogChanged(catalog) => projection.catalog.clone_from(catalog),
        SessionEvent::PersistenceWarning(warning) => {
            projection.persistence_warning.clone_from(warning);
        }
        SessionEvent::Deleted { .. } => {
            return Err(ClientSessionError::message(
                "deleted session event requires startup fallback",
            ));
        }
    }
    projection.sequence = envelope.sequence;
    Ok(())
}

fn refresh_running_summary(projection: &mut SessionSnapshot) {
    projection.summary.running_jobs = u32::try_from(
        projection
            .jobs
            .iter()
            .filter(|job| job.state == JobState::Running)
            .count(),
    )
    .unwrap_or(u32::MAX);
    projection.summary.running =
        projection.active_run.is_some() || projection.summary.running_jobs > 0;
}

fn validate_event_run(
    projection: &SessionSnapshot,
    event: &SessionEvent,
) -> Result<(), ClientSessionError> {
    match event {
        SessionEvent::Started { .. } if projection.active_run.is_some() => Err(
            ClientSessionError::message("a run started while another was active"),
        ),
        SessionEvent::Started { .. }
        | SessionEvent::TitleChanged { .. }
        | SessionEvent::SettingsChanged { .. }
        | SessionEvent::JobsChanged(_)
        | SessionEvent::PlanChanged(_)
        | SessionEvent::CatalogChanged(_)
        | SessionEvent::PersistenceWarning(_) => Ok(()),
        SessionEvent::Deleted { .. } => Err(ClientSessionError::message(
            "deleted session event requires startup fallback",
        )),
        SessionEvent::AssistantDelta { run_id, .. }
        | SessionEvent::ContextUsage { run_id, .. }
        | SessionEvent::ToolStarted { run_id, .. }
        | SessionEvent::ToolFinished { run_id, .. }
        | SessionEvent::Completed { run_id, .. }
        | SessionEvent::Failed { run_id, .. }
        | SessionEvent::Cancelled { run_id } => match &projection.active_run {
            Some(active) if active.run_id == *run_id => Ok(()),
            Some(_) => Err(ClientSessionError::message(
                "session event run identifier does not match",
            )),
            None => Err(ClientSessionError::message(
                "session run event arrived while idle",
            )),
        },
    }
}

impl WorkspaceSession for RpcSessionClient {
    fn snapshot(&self) -> &SessionSnapshot {
        RpcSessionClient::snapshot(self)
    }

    async fn next_update(&mut self) -> Result<SessionUpdate, ClientSessionError> {
        RpcSessionClient::next_update(self)
            .await
            .map_err(Into::into)
    }

    async fn submit(&self, prompt: String) -> Result<u64, ClientSessionError> {
        RpcSessionClient::submit(self, prompt)
            .await
            .map_err(Into::into)
    }

    async fn cancel(&self) -> Result<(), ClientSessionError> {
        RpcSessionClient::cancel(self).await.map_err(Into::into)
    }

    async fn select_model(&self, model: String) -> Result<(), ClientSessionError> {
        RpcSessionClient::select_model(self, model)
            .await
            .map_err(Into::into)
    }

    async fn select_reasoning(&self, reasoning: ReasoningLevel) -> Result<(), ClientSessionError> {
        RpcSessionClient::select_reasoning(self, reasoning)
            .await
            .map_err(Into::into)
    }

    async fn list_jobs(&self) -> Result<Vec<JobSnapshotDto>, ClientSessionError> {
        RpcSessionClient::list_jobs(self).await.map_err(Into::into)
    }

    async fn cancel_job(&self, id: String) -> Result<JobSnapshotDto, ClientSessionError> {
        RpcSessionClient::cancel_job(self, id)
            .await
            .map_err(Into::into)
    }

    async fn detach(self) -> Result<(), ClientSessionError> {
        RpcSessionClient::detach(self).await.map_err(Into::into)
    }
}

impl WorkspaceBackend for RpcBackendClient {
    type Session = RpcSessionClient;

    async fn draft_defaults(&self, cwd: Vec<u8>) -> Result<DraftState, ClientSessionError> {
        Ok(RpcBackendClient::draft_defaults(self, cwd).await?.into())
    }

    async fn startup(
        &self,
        cwd: Vec<u8>,
    ) -> Result<WorkspaceStartup<Self::Session>, ClientSessionError> {
        match RpcBackendClient::startup(self, cwd).await? {
            RpcStartup::Draft(defaults) => Ok(WorkspaceStartup::Draft(defaults.into())),
            RpcStartup::Attached(session) => Ok(WorkspaceStartup::Attached(*session)),
        }
    }

    async fn materialize(
        &self,
        cwd: Vec<u8>,
        prompt: String,
        settings: SessionSettings,
    ) -> Result<WorkspaceMaterialized<Self::Session>, ClientSessionError> {
        let MaterializedSession { session, run_id } =
            RpcBackendClient::materialize(self, cwd, prompt, settings).await?;
        Ok(WorkspaceMaterialized { session, run_id })
    }

    async fn open_session(
        &self,
        selector: SessionSelector,
        cwd_for_title: Vec<u8>,
    ) -> Result<Self::Session, ClientSessionError> {
        RpcBackendClient::open_session(self, selector, cwd_for_title)
            .await
            .map_err(Into::into)
    }

    fn list_sessions(&self, scope: SessionListScope) -> SessionListFuture {
        let future = RpcBackendClient::list_sessions(self, scope);
        Box::pin(async move { future.await.map_err(Into::into) })
    }

    async fn rename_session(
        &self,
        session_id: SessionId,
        title: SessionTitle,
    ) -> Result<(), ClientSessionError> {
        RpcBackendClient::rename_session(self, session_id, title)
            .await
            .map_err(Into::into)
    }

    async fn delete_session(&self, session_id: SessionId) -> Result<(), ClientSessionError> {
        RpcBackendClient::delete_session(self, session_id)
            .await
            .map_err(Into::into)
    }

    async fn disconnect(self) -> Result<(), ClientSessionError> {
        RpcBackendClient::disconnect(self).await.map_err(Into::into)
    }
}
