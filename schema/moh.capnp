@0x9ea0e1de9de6bd37;

const protocolMajor :UInt16 = 2;
const protocolMinor :UInt16 = 0;

interface Backend {
  getInfo @0 () -> (info :ProtocolInfo);
  startup @1 (cwd :Data, attachmentId :UInt64, observer :Observer)
      -> (result :StartupResult);
  materialize @2 (cwd :Data, prompt :Text, settings :SessionSettings,
      attachmentId :UInt64, observer :Observer) -> (result :MaterializeResult);
  openSession @3 (selector :SessionSelector, cwdForTitle :Data,
      attachmentId :UInt64, observer :Observer) -> (result :OpenResult);
  listSessions @4 (scope :SessionListScope, cwd :Data)
      -> (result :SessionListResult);
  renameSession @5 (id :Text, title :Text) -> (result :CommandResult);
  deleteSession @6 (id :Text) -> (result :CommandResult);
  draftDefaults @7 (cwd :Data) -> (result :DraftDefaultsResult);
}

interface Session {
  submit @0 (prompt :Text) -> (result :SubmitResult);
  cancel @1 () -> (result :CommandResult);
  selectModel @2 (modelId :Text) -> (result :CommandResult);
  selectReasoning @3 (level :ReasoningLevel) -> (result :CommandResult);
  listJobs @4 () -> (result :JobListResult);
  cancelJob @5 (jobId :Text) -> (result :JobResult);
  detach @6 (attachmentId :UInt64) -> (result :CommandResult);
}

interface Observer {
  publish @0 (event :EventEnvelope) -> ();
}

struct ProtocolInfo {
  major @0 :UInt16;
  minor @1 :UInt16;
  instanceId @2 :Text;
  startupWarnings @3 :List(Text);
  features @4 :List(Text);
}

struct SessionSelector {
  union {
    id @0 :Text;
    title @1 :Text;
  }
}

enum SessionListScope {
  project @0;
  all @1;
}

enum ErrorCode {
  busy @0;
  notRunning @1;
  sessionNotFound @2;
  sessionNameConflict @3;
  invalidArgument @4;
  modelNotFound @5;
  unsupportedReasoning @6;
  jobNotFound @7;
  backendStarting @8;
  backendUnavailable @9;
  persistence @10;
  internal @11;
  ambiguousTitle @12;
  sessionDeleting @13;
  sessionDeleted @14;
}

struct CommandError {
  code @0 :ErrorCode;
  message @1 :Text;
  ids @2 :List(Text);
}

struct DraftDefaults {
  cwd @0 :Data;
  settings @1 :SessionSettings;
  catalog @2 :ModelCatalog;
}

struct DraftDefaultsResult {
  union {
    defaults @0 :DraftDefaults;
    error @1 :CommandError;
  }
}

struct StartupResult {
  union {
    draft @0 :DraftDefaults;
    attached @1 :OpenSuccess;
    error @2 :CommandError;
  }
}

struct MaterializeResult {
  union {
    success @0 :MaterializeSuccess;
    error @1 :CommandError;
  }
}

struct MaterializeSuccess {
  session @0 :Session;
  snapshot @1 :SessionSnapshot;
  runId @2 :UInt64;
}

struct OpenResult {
  union {
    success @0 :OpenSuccess;
    error @1 :CommandError;
  }
}

struct OpenSuccess {
  session @0 :Session;
  snapshot @1 :SessionSnapshot;
}

struct SessionListResult {
  union {
    sessions @0 :List(SessionSummary);
    error @1 :CommandError;
  }
}

struct SubmitResult {
  union {
    runId @0 :UInt64;
    error @1 :CommandError;
  }
}

struct CommandResult {
  union {
    ok @0 :Void;
    error @1 :CommandError;
  }
  attachedClients @2 :UInt32;
}

struct JobListResult {
  union {
    jobs @0 :List(JobSnapshot);
    error @1 :CommandError;
  }
}

struct JobResult {
  union {
    job @0 :JobSnapshot;
    error @1 :CommandError;
  }
}

enum ReasoningLevel {
  none @0;
  minimal @1;
  low @2;
  medium @3;
  high @4;
  xhigh @5;
  max @6;
}

struct SessionSettings {
  model @0 :Text;
  reasoning @1 :ReasoningLevel;
  contextTokens @2 :UInt64;
}

enum PlanStatus {
  pending @0;
  inProgress @1;
  completed @2;
  blocked @3;
  cancelled @4;
}

struct PlanItem {
  step @0 :Text;
  status @1 :PlanStatus;
}

struct SessionSummary {
  id @0 :Text;
  title @1 :Text;
  cwd @2 :Data;
  cwdDisplay @3 :Text;
  titleRevision @4 :UInt64;
  busy @5 :Bool;
  attachedClients @6 :UInt32;
  lastActivity @7 :Text;
  running @8 :Bool;
  runningJobs @9 :UInt32;
}

struct ActiveRun {
  runId @0 :UInt64;
  prompt @1 :Text;
  assistantText @2 :Text;
}

struct ToolStartedRecord {
  runId @0 :UInt64;
  callId @1 :Text;
  name @2 :Text;
  argumentsJson @3 :Text;
}

struct FailedRecord {
  runId @0 :UInt64;
  failure @1 :RunFailure;
}

struct TranscriptItem {
  union {
    user @0 :Text;
    assistant @1 :Text;
    toolStarted @2 :ToolStartedRecord;
    failed @3 :FailedRecord;
    cancelledRunId @4 :UInt64;
  }
}

struct ModelInfo {
  id @0 :Text;
  displayName @1 :Text;
  description @2 :Text;
  reasoningEfforts @3 :List(ReasoningLevel);
  hasDefaultReasoning @4 :Bool;
  defaultReasoning @5 :ReasoningLevel;
}

struct ModelCatalog {
  union {
    loading @0 :Void;
    ready @1 :List(ModelInfo);
    failed @2 :Text;
  }
}

enum JobKind {
  bash @0;
}

enum JobState {
  running @0;
  completed @1;
  failed @2;
  cancelled @3;
}

struct JobSnapshot {
  id @0 :Text;
  kind @1 :JobKind;
  state @2 :JobState;
  title @3 :Text;
  startedAt @4 :Text;
  completedAt @5 :Text;
  details @6 :Text;
}

enum RunStage {
  startup @0;
  modelRequest @1;
  toolExecution @2;
  finalization @3;
}

enum RunFailureKind {
  authentication @0;
  transport @1;
  httpRejected @2;
  protocol @3;
  emptyResponse @4;
  budgetExhausted @5;
  runtimeInfrastructure @6;
  toolInfrastructure @7;
}

struct RunFailure {
  stage @0 :RunStage;
  kind @1 :RunFailureKind;
  hasHttpStatus @2 :Bool;
  httpStatus @3 :UInt16;
  retryable @4 :Bool;
  message @5 :Text;
}

struct SessionSnapshot {
  summary @0 :SessionSummary;
  transcript @1 :List(TranscriptItem);
  activeRun @2 :ActiveRun;
  settings @3 :SessionSettings;
  catalog @4 :ModelCatalog;
  plan @9 :List(PlanItem);
  jobs @5 :List(JobSnapshot);
  persistenceWarning @6 :Text;
  sequence @7 :UInt64;
  busy @8 :Bool;
}

struct RunStarted {
  runId @0 :UInt64;
  prompt @1 :Text;
}

struct AssistantDelta {
  runId @0 :UInt64;
  text @1 :Text;
}

struct ContextUsage {
  runId @0 :UInt64;
  inputTokens @1 :UInt64;
  lastActivity @2 :Text;
}

struct ToolFinished {
  runId @0 :UInt64;
  callId @1 :Text;
  name @2 :Text;
}

struct RunCompleted {
  runId @0 :UInt64;
  response @1 :Text;
  lastActivity @2 :Text;
}

struct RunFailed {
  runId @0 :UInt64;
  failure @1 :RunFailure;
}

struct SettingsChanged {
  settings @0 :SessionSettings;
  lastActivity @1 :Text;
}

struct TitleChanged {
  title @0 :Text;
  titleRevision @1 :UInt64;
}

struct SessionDeleted {
  sessionId @0 :Text;
}

struct EventEnvelope {
  sequence @0 :UInt64;
  union {
    started @1 :RunStarted;
    assistantDelta @2 :AssistantDelta;
    contextUsage @3 :ContextUsage;
    toolStarted @4 :ToolStartedRecord;
    toolFinished @5 :ToolFinished;
    completed @6 :RunCompleted;
    failed @7 :RunFailed;
    cancelledRunId @8 :UInt64;
    settingsChanged @9 :SettingsChanged;
    jobsChanged @10 :List(JobSnapshot);
    catalogChanged @11 :ModelCatalog;
    persistenceWarning @12 :Text;
    titleChanged @13 :TitleChanged;
    deleted @14 :SessionDeleted;
    planChanged @15 :List(PlanItem);
  }
}
