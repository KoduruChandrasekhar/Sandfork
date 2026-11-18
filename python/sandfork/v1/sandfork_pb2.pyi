import datetime

from google.protobuf import duration_pb2 as _duration_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ErrorClass(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    ERROR_CLASS_UNSPECIFIED: _ClassVar[ErrorClass]
    TASK: _ClassVar[ErrorClass]
    INFRA: _ClassVar[ErrorClass]
    CLIENT: _ClassVar[ErrorClass]

class ForkMode(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    AUTO: _ClassVar[ForkMode]
    F1_COLD: _ClassVar[ForkMode]
    F2_FS: _ClassVar[ForkMode]
    F3_ZYGOTE: _ClassVar[ForkMode]
    F4_RESTORE: _ClassVar[ForkMode]
ERROR_CLASS_UNSPECIFIED: ErrorClass
TASK: ErrorClass
INFRA: ErrorClass
CLIENT: ErrorClass
AUTO: ForkMode
F1_COLD: ForkMode
F2_FS: ForkMode
F3_ZYGOTE: ForkMode
F4_RESTORE: ForkMode

class Empty(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...

class Error(_message.Message):
    __slots__ = ("code", "message")
    CLASS_FIELD_NUMBER: _ClassVar[int]
    CODE_FIELD_NUMBER: _ClassVar[int]
    MESSAGE_FIELD_NUMBER: _ClassVar[int]
    code: str
    message: str
    def __init__(self, code: _Optional[str] = ..., message: _Optional[str] = ..., **kwargs) -> None: ...

class Limits(_message.Message):
    __slots__ = ("memory_mb", "pids", "cpu", "disk_mb")
    MEMORY_MB_FIELD_NUMBER: _ClassVar[int]
    PIDS_FIELD_NUMBER: _ClassVar[int]
    CPU_FIELD_NUMBER: _ClassVar[int]
    DISK_MB_FIELD_NUMBER: _ClassVar[int]
    memory_mb: int
    pids: int
    cpu: float
    disk_mb: int
    def __init__(self, memory_mb: _Optional[int] = ..., pids: _Optional[int] = ..., cpu: _Optional[float] = ..., disk_mb: _Optional[int] = ...) -> None: ...

class PrepareTaskRequest(_message.Message):
    __slots__ = ("request_id", "task_dir", "force")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    TASK_DIR_FIELD_NUMBER: _ClassVar[int]
    FORCE_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    task_dir: str
    force: bool
    def __init__(self, request_id: _Optional[str] = ..., task_dir: _Optional[str] = ..., force: _Optional[bool] = ...) -> None: ...

class Snapshot(_message.Message):
    __slots__ = ("id", "task_id", "kind", "layers", "parent", "created_unix", "prepare_micros", "cached", "bytes_written", "spec_hash")
    ID_FIELD_NUMBER: _ClassVar[int]
    TASK_ID_FIELD_NUMBER: _ClassVar[int]
    KIND_FIELD_NUMBER: _ClassVar[int]
    LAYERS_FIELD_NUMBER: _ClassVar[int]
    PARENT_FIELD_NUMBER: _ClassVar[int]
    CREATED_UNIX_FIELD_NUMBER: _ClassVar[int]
    PREPARE_MICROS_FIELD_NUMBER: _ClassVar[int]
    CACHED_FIELD_NUMBER: _ClassVar[int]
    BYTES_WRITTEN_FIELD_NUMBER: _ClassVar[int]
    SPEC_HASH_FIELD_NUMBER: _ClassVar[int]
    id: str
    task_id: str
    kind: str
    layers: _containers.RepeatedScalarFieldContainer[str]
    parent: str
    created_unix: int
    prepare_micros: int
    cached: bool
    bytes_written: int
    spec_hash: str
    def __init__(self, id: _Optional[str] = ..., task_id: _Optional[str] = ..., kind: _Optional[str] = ..., layers: _Optional[_Iterable[str]] = ..., parent: _Optional[str] = ..., created_unix: _Optional[int] = ..., prepare_micros: _Optional[int] = ..., cached: _Optional[bool] = ..., bytes_written: _Optional[int] = ..., spec_hash: _Optional[str] = ...) -> None: ...

class ForkGroupRequest(_message.Message):
    __slots__ = ("request_id", "snapshot_id", "n", "mode", "limits", "admission_deadline", "lease", "labels")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    SNAPSHOT_ID_FIELD_NUMBER: _ClassVar[int]
    N_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    LIMITS_FIELD_NUMBER: _ClassVar[int]
    ADMISSION_DEADLINE_FIELD_NUMBER: _ClassVar[int]
    LEASE_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    snapshot_id: str
    n: int
    mode: ForkMode
    limits: Limits
    admission_deadline: _duration_pb2.Duration
    lease: _duration_pb2.Duration
    labels: _containers.ScalarMap[str, str]
    def __init__(self, request_id: _Optional[str] = ..., snapshot_id: _Optional[str] = ..., n: _Optional[int] = ..., mode: _Optional[_Union[ForkMode, str]] = ..., limits: _Optional[_Union[Limits, _Mapping]] = ..., admission_deadline: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ..., lease: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ..., labels: _Optional[_Mapping[str, str]] = ...) -> None: ...

class StepTiming(_message.Message):
    __slots__ = ("name", "micros")
    NAME_FIELD_NUMBER: _ClassVar[int]
    MICROS_FIELD_NUMBER: _ClassVar[int]
    name: str
    micros: int
    def __init__(self, name: _Optional[str] = ..., micros: _Optional[int] = ...) -> None: ...

class Sandbox(_message.Message):
    __slots__ = ("id", "group_id", "task_id", "mode", "level", "fork_micros", "steps", "snapshot_id")
    ID_FIELD_NUMBER: _ClassVar[int]
    GROUP_ID_FIELD_NUMBER: _ClassVar[int]
    TASK_ID_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    LEVEL_FIELD_NUMBER: _ClassVar[int]
    FORK_MICROS_FIELD_NUMBER: _ClassVar[int]
    STEPS_FIELD_NUMBER: _ClassVar[int]
    SNAPSHOT_ID_FIELD_NUMBER: _ClassVar[int]
    id: str
    group_id: str
    task_id: str
    mode: ForkMode
    level: str
    fork_micros: int
    steps: _containers.RepeatedCompositeFieldContainer[StepTiming]
    snapshot_id: str
    def __init__(self, id: _Optional[str] = ..., group_id: _Optional[str] = ..., task_id: _Optional[str] = ..., mode: _Optional[_Union[ForkMode, str]] = ..., level: _Optional[str] = ..., fork_micros: _Optional[int] = ..., steps: _Optional[_Iterable[_Union[StepTiming, _Mapping]]] = ..., snapshot_id: _Optional[str] = ...) -> None: ...

class Lease(_message.Message):
    __slots__ = ("group_id", "expires_unix_ms", "token")
    GROUP_ID_FIELD_NUMBER: _ClassVar[int]
    EXPIRES_UNIX_MS_FIELD_NUMBER: _ClassVar[int]
    TOKEN_FIELD_NUMBER: _ClassVar[int]
    group_id: str
    expires_unix_ms: int
    token: int
    def __init__(self, group_id: _Optional[str] = ..., expires_unix_ms: _Optional[int] = ..., token: _Optional[int] = ...) -> None: ...

class Group(_message.Message):
    __slots__ = ("id", "sandboxes", "lease", "queued_micros", "fork_micros", "mode", "task_id")
    ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOXES_FIELD_NUMBER: _ClassVar[int]
    LEASE_FIELD_NUMBER: _ClassVar[int]
    QUEUED_MICROS_FIELD_NUMBER: _ClassVar[int]
    FORK_MICROS_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    TASK_ID_FIELD_NUMBER: _ClassVar[int]
    id: str
    sandboxes: _containers.RepeatedCompositeFieldContainer[Sandbox]
    lease: Lease
    queued_micros: int
    fork_micros: int
    mode: ForkMode
    task_id: str
    def __init__(self, id: _Optional[str] = ..., sandboxes: _Optional[_Iterable[_Union[Sandbox, _Mapping]]] = ..., lease: _Optional[_Union[Lease, _Mapping]] = ..., queued_micros: _Optional[int] = ..., fork_micros: _Optional[int] = ..., mode: _Optional[_Union[ForkMode, str]] = ..., task_id: _Optional[str] = ...) -> None: ...

class ForkRequest(_message.Message):
    __slots__ = ("request_id", "snapshot_id", "sandbox_id", "n", "limits", "admission_deadline", "lease", "labels")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    SNAPSHOT_ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    N_FIELD_NUMBER: _ClassVar[int]
    LIMITS_FIELD_NUMBER: _ClassVar[int]
    ADMISSION_DEADLINE_FIELD_NUMBER: _ClassVar[int]
    LEASE_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    snapshot_id: str
    sandbox_id: str
    n: int
    limits: Limits
    admission_deadline: _duration_pb2.Duration
    lease: _duration_pb2.Duration
    labels: _containers.ScalarMap[str, str]
    def __init__(self, request_id: _Optional[str] = ..., snapshot_id: _Optional[str] = ..., sandbox_id: _Optional[str] = ..., n: _Optional[int] = ..., limits: _Optional[_Union[Limits, _Mapping]] = ..., admission_deadline: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ..., lease: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ..., labels: _Optional[_Mapping[str, str]] = ...) -> None: ...

class ReleaseRequest(_message.Message):
    __slots__ = ("group_id", "sandbox_id")
    GROUP_ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    group_id: str
    sandbox_id: str
    def __init__(self, group_id: _Optional[str] = ..., sandbox_id: _Optional[str] = ...) -> None: ...

class RenewLeaseRequest(_message.Message):
    __slots__ = ("group_id", "extend", "token")
    GROUP_ID_FIELD_NUMBER: _ClassVar[int]
    EXTEND_FIELD_NUMBER: _ClassVar[int]
    TOKEN_FIELD_NUMBER: _ClassVar[int]
    group_id: str
    extend: _duration_pb2.Duration
    token: int
    def __init__(self, group_id: _Optional[str] = ..., extend: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ..., token: _Optional[int] = ...) -> None: ...

class Argv(_message.Message):
    __slots__ = ("args",)
    ARGS_FIELD_NUMBER: _ClassVar[int]
    args: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, args: _Optional[_Iterable[str]] = ...) -> None: ...

class ExecRequest(_message.Message):
    __slots__ = ("request_id", "sandbox_id", "shell", "argv", "cwd", "timeout", "max_output_bytes", "env", "stdin")
    class EnvEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    SHELL_FIELD_NUMBER: _ClassVar[int]
    ARGV_FIELD_NUMBER: _ClassVar[int]
    CWD_FIELD_NUMBER: _ClassVar[int]
    TIMEOUT_FIELD_NUMBER: _ClassVar[int]
    MAX_OUTPUT_BYTES_FIELD_NUMBER: _ClassVar[int]
    ENV_FIELD_NUMBER: _ClassVar[int]
    STDIN_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    sandbox_id: str
    shell: str
    argv: Argv
    cwd: str
    timeout: _duration_pb2.Duration
    max_output_bytes: int
    env: _containers.ScalarMap[str, str]
    stdin: bytes
    def __init__(self, request_id: _Optional[str] = ..., sandbox_id: _Optional[str] = ..., shell: _Optional[str] = ..., argv: _Optional[_Union[Argv, _Mapping]] = ..., cwd: _Optional[str] = ..., timeout: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ..., max_output_bytes: _Optional[int] = ..., env: _Optional[_Mapping[str, str]] = ..., stdin: _Optional[bytes] = ...) -> None: ...

class ExecExit(_message.Message):
    __slots__ = ("exit_code", "signal", "timed_out", "wall_ms", "user_ms", "sys_ms", "max_rss_kb", "truncated", "dropped_bytes", "tail_stdout", "tail_stderr", "oom_killed", "cancelled")
    EXIT_CODE_FIELD_NUMBER: _ClassVar[int]
    SIGNAL_FIELD_NUMBER: _ClassVar[int]
    TIMED_OUT_FIELD_NUMBER: _ClassVar[int]
    WALL_MS_FIELD_NUMBER: _ClassVar[int]
    USER_MS_FIELD_NUMBER: _ClassVar[int]
    SYS_MS_FIELD_NUMBER: _ClassVar[int]
    MAX_RSS_KB_FIELD_NUMBER: _ClassVar[int]
    TRUNCATED_FIELD_NUMBER: _ClassVar[int]
    DROPPED_BYTES_FIELD_NUMBER: _ClassVar[int]
    TAIL_STDOUT_FIELD_NUMBER: _ClassVar[int]
    TAIL_STDERR_FIELD_NUMBER: _ClassVar[int]
    OOM_KILLED_FIELD_NUMBER: _ClassVar[int]
    CANCELLED_FIELD_NUMBER: _ClassVar[int]
    exit_code: int
    signal: int
    timed_out: bool
    wall_ms: int
    user_ms: int
    sys_ms: int
    max_rss_kb: int
    truncated: bool
    dropped_bytes: int
    tail_stdout: bytes
    tail_stderr: bytes
    oom_killed: bool
    cancelled: bool
    def __init__(self, exit_code: _Optional[int] = ..., signal: _Optional[int] = ..., timed_out: _Optional[bool] = ..., wall_ms: _Optional[int] = ..., user_ms: _Optional[int] = ..., sys_ms: _Optional[int] = ..., max_rss_kb: _Optional[int] = ..., truncated: _Optional[bool] = ..., dropped_bytes: _Optional[int] = ..., tail_stdout: _Optional[bytes] = ..., tail_stderr: _Optional[bytes] = ..., oom_killed: _Optional[bool] = ..., cancelled: _Optional[bool] = ...) -> None: ...

class ExecEvent(_message.Message):
    __slots__ = ("stdout", "stderr", "exit")
    STDOUT_FIELD_NUMBER: _ClassVar[int]
    STDERR_FIELD_NUMBER: _ClassVar[int]
    EXIT_FIELD_NUMBER: _ClassVar[int]
    stdout: bytes
    stderr: bytes
    exit: ExecExit
    def __init__(self, stdout: _Optional[bytes] = ..., stderr: _Optional[bytes] = ..., exit: _Optional[_Union[ExecExit, _Mapping]] = ...) -> None: ...

class ReadFileRequest(_message.Message):
    __slots__ = ("sandbox_id", "path", "offset", "max_bytes")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    OFFSET_FIELD_NUMBER: _ClassVar[int]
    MAX_BYTES_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    path: str
    offset: int
    max_bytes: int
    def __init__(self, sandbox_id: _Optional[str] = ..., path: _Optional[str] = ..., offset: _Optional[int] = ..., max_bytes: _Optional[int] = ...) -> None: ...

class FileChunk(_message.Message):
    __slots__ = ("data", "eof", "size")
    DATA_FIELD_NUMBER: _ClassVar[int]
    EOF_FIELD_NUMBER: _ClassVar[int]
    SIZE_FIELD_NUMBER: _ClassVar[int]
    data: bytes
    eof: bool
    size: int
    def __init__(self, data: _Optional[bytes] = ..., eof: _Optional[bool] = ..., size: _Optional[int] = ...) -> None: ...

class WriteFileRequest(_message.Message):
    __slots__ = ("request_id", "sandbox_id", "path", "data", "mode", "append")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    DATA_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    APPEND_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    sandbox_id: str
    path: str
    data: bytes
    mode: int
    append: bool
    def __init__(self, request_id: _Optional[str] = ..., sandbox_id: _Optional[str] = ..., path: _Optional[str] = ..., data: _Optional[bytes] = ..., mode: _Optional[int] = ..., append: _Optional[bool] = ...) -> None: ...

class WriteFileResult(_message.Message):
    __slots__ = ("bytes_written",)
    BYTES_WRITTEN_FIELD_NUMBER: _ClassVar[int]
    bytes_written: int
    def __init__(self, bytes_written: _Optional[int] = ...) -> None: ...

class EvalRequest(_message.Message):
    __slots__ = ("request_id", "sandbox_id", "code", "timeout")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    CODE_FIELD_NUMBER: _ClassVar[int]
    TIMEOUT_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    sandbox_id: str
    code: str
    timeout: _duration_pb2.Duration
    def __init__(self, request_id: _Optional[str] = ..., sandbox_id: _Optional[str] = ..., code: _Optional[str] = ..., timeout: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ...) -> None: ...

class EvalResult(_message.Message):
    __slots__ = ("ok", "output")
    OK_FIELD_NUMBER: _ClassVar[int]
    OUTPUT_FIELD_NUMBER: _ClassVar[int]
    ok: bool
    output: str
    def __init__(self, ok: _Optional[bool] = ..., output: _Optional[str] = ...) -> None: ...

class SnapshotRequest(_message.Message):
    __slots__ = ("request_id", "sandbox_id", "kind")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    KIND_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    sandbox_id: str
    kind: str
    def __init__(self, request_id: _Optional[str] = ..., sandbox_id: _Optional[str] = ..., kind: _Optional[str] = ...) -> None: ...

class CheckpointRequest(_message.Message):
    __slots__ = ("request_id", "sandbox_id", "turn")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    TURN_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    sandbox_id: str
    turn: int
    def __init__(self, request_id: _Optional[str] = ..., sandbox_id: _Optional[str] = ..., turn: _Optional[int] = ...) -> None: ...

class Checkpoint(_message.Message):
    __slots__ = ("id", "sandbox_id", "snapshot_id", "turn", "has_process_image", "bytes_written", "micros")
    ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    SNAPSHOT_ID_FIELD_NUMBER: _ClassVar[int]
    TURN_FIELD_NUMBER: _ClassVar[int]
    HAS_PROCESS_IMAGE_FIELD_NUMBER: _ClassVar[int]
    BYTES_WRITTEN_FIELD_NUMBER: _ClassVar[int]
    MICROS_FIELD_NUMBER: _ClassVar[int]
    id: str
    sandbox_id: str
    snapshot_id: str
    turn: int
    has_process_image: bool
    bytes_written: int
    micros: int
    def __init__(self, id: _Optional[str] = ..., sandbox_id: _Optional[str] = ..., snapshot_id: _Optional[str] = ..., turn: _Optional[int] = ..., has_process_image: _Optional[bool] = ..., bytes_written: _Optional[int] = ..., micros: _Optional[int] = ...) -> None: ...

class RestoreRequest(_message.Message):
    __slots__ = ("request_id", "checkpoint_id", "n", "admission_deadline", "lease", "labels")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    CHECKPOINT_ID_FIELD_NUMBER: _ClassVar[int]
    N_FIELD_NUMBER: _ClassVar[int]
    ADMISSION_DEADLINE_FIELD_NUMBER: _ClassVar[int]
    LEASE_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    checkpoint_id: str
    n: int
    admission_deadline: _duration_pb2.Duration
    lease: _duration_pb2.Duration
    labels: _containers.ScalarMap[str, str]
    def __init__(self, request_id: _Optional[str] = ..., checkpoint_id: _Optional[str] = ..., n: _Optional[int] = ..., admission_deadline: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ..., lease: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ..., labels: _Optional[_Mapping[str, str]] = ...) -> None: ...

class VerifyRequest(_message.Message):
    __slots__ = ("request_id", "sandbox_id")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    sandbox_id: str
    def __init__(self, request_id: _Optional[str] = ..., sandbox_id: _Optional[str] = ...) -> None: ...

class DiffStats(_message.Message):
    __slots__ = ("files", "lines_added", "lines_removed", "rejected_paths", "flags")
    FILES_FIELD_NUMBER: _ClassVar[int]
    LINES_ADDED_FIELD_NUMBER: _ClassVar[int]
    LINES_REMOVED_FIELD_NUMBER: _ClassVar[int]
    REJECTED_PATHS_FIELD_NUMBER: _ClassVar[int]
    FLAGS_FIELD_NUMBER: _ClassVar[int]
    files: int
    lines_added: int
    lines_removed: int
    rejected_paths: _containers.RepeatedScalarFieldContainer[str]
    flags: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, files: _Optional[int] = ..., lines_added: _Optional[int] = ..., lines_removed: _Optional[int] = ..., rejected_paths: _Optional[_Iterable[str]] = ..., flags: _Optional[_Iterable[str]] = ...) -> None: ...

class VerifyResult(_message.Message):
    __slots__ = ("status", "reward", "passed", "total", "verifier_version", "duration_ms", "diff", "error", "output_tail")
    STATUS_FIELD_NUMBER: _ClassVar[int]
    REWARD_FIELD_NUMBER: _ClassVar[int]
    PASSED_FIELD_NUMBER: _ClassVar[int]
    TOTAL_FIELD_NUMBER: _ClassVar[int]
    VERIFIER_VERSION_FIELD_NUMBER: _ClassVar[int]
    DURATION_MS_FIELD_NUMBER: _ClassVar[int]
    DIFF_FIELD_NUMBER: _ClassVar[int]
    ERROR_FIELD_NUMBER: _ClassVar[int]
    OUTPUT_TAIL_FIELD_NUMBER: _ClassVar[int]
    status: str
    reward: float
    passed: int
    total: int
    verifier_version: str
    duration_ms: int
    diff: DiffStats
    error: Error
    output_tail: str
    def __init__(self, status: _Optional[str] = ..., reward: _Optional[float] = ..., passed: _Optional[int] = ..., total: _Optional[int] = ..., verifier_version: _Optional[str] = ..., duration_ms: _Optional[int] = ..., diff: _Optional[_Union[DiffStats, _Mapping]] = ..., error: _Optional[_Union[Error, _Mapping]] = ..., output_tail: _Optional[str] = ...) -> None: ...

class DiffRequest(_message.Message):
    __slots__ = ("sandbox_id", "text")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    text: bool
    def __init__(self, sandbox_id: _Optional[str] = ..., text: _Optional[bool] = ...) -> None: ...

class FileChange(_message.Message):
    __slots__ = ("path", "kind", "is_dir", "size", "text_diff")
    PATH_FIELD_NUMBER: _ClassVar[int]
    KIND_FIELD_NUMBER: _ClassVar[int]
    IS_DIR_FIELD_NUMBER: _ClassVar[int]
    SIZE_FIELD_NUMBER: _ClassVar[int]
    TEXT_DIFF_FIELD_NUMBER: _ClassVar[int]
    path: str
    kind: str
    is_dir: bool
    size: int
    text_diff: str
    def __init__(self, path: _Optional[str] = ..., kind: _Optional[str] = ..., is_dir: _Optional[bool] = ..., size: _Optional[int] = ..., text_diff: _Optional[str] = ...) -> None: ...

class DiffResult(_message.Message):
    __slots__ = ("changes", "lines_added", "lines_removed")
    CHANGES_FIELD_NUMBER: _ClassVar[int]
    LINES_ADDED_FIELD_NUMBER: _ClassVar[int]
    LINES_REMOVED_FIELD_NUMBER: _ClassVar[int]
    changes: _containers.RepeatedCompositeFieldContainer[FileChange]
    lines_added: int
    lines_removed: int
    def __init__(self, changes: _Optional[_Iterable[_Union[FileChange, _Mapping]]] = ..., lines_added: _Optional[int] = ..., lines_removed: _Optional[int] = ...) -> None: ...

class ListSandboxesRequest(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...

class SandboxInfo(_message.Message):
    __slots__ = ("id", "group_id", "task_id", "mode", "state", "age_ms", "memory_bytes", "purpose", "execs")
    ID_FIELD_NUMBER: _ClassVar[int]
    GROUP_ID_FIELD_NUMBER: _ClassVar[int]
    TASK_ID_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    STATE_FIELD_NUMBER: _ClassVar[int]
    AGE_MS_FIELD_NUMBER: _ClassVar[int]
    MEMORY_BYTES_FIELD_NUMBER: _ClassVar[int]
    PURPOSE_FIELD_NUMBER: _ClassVar[int]
    EXECS_FIELD_NUMBER: _ClassVar[int]
    id: str
    group_id: str
    task_id: str
    mode: ForkMode
    state: str
    age_ms: int
    memory_bytes: int
    purpose: str
    execs: int
    def __init__(self, id: _Optional[str] = ..., group_id: _Optional[str] = ..., task_id: _Optional[str] = ..., mode: _Optional[_Union[ForkMode, str]] = ..., state: _Optional[str] = ..., age_ms: _Optional[int] = ..., memory_bytes: _Optional[int] = ..., purpose: _Optional[str] = ..., execs: _Optional[int] = ...) -> None: ...

class SandboxList(_message.Message):
    __slots__ = ("sandboxes",)
    SANDBOXES_FIELD_NUMBER: _ClassVar[int]
    sandboxes: _containers.RepeatedCompositeFieldContainer[SandboxInfo]
    def __init__(self, sandboxes: _Optional[_Iterable[_Union[SandboxInfo, _Mapping]]] = ...) -> None: ...

class RegisterTasksRequest(_message.Message):
    __slots__ = ("task_dirs",)
    TASK_DIRS_FIELD_NUMBER: _ClassVar[int]
    task_dirs: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, task_dirs: _Optional[_Iterable[str]] = ...) -> None: ...

class RegisterTasksResult(_message.Message):
    __slots__ = ("registered", "errors")
    REGISTERED_FIELD_NUMBER: _ClassVar[int]
    ERRORS_FIELD_NUMBER: _ClassVar[int]
    registered: int
    errors: _containers.RepeatedCompositeFieldContainer[Error]
    def __init__(self, registered: _Optional[int] = ..., errors: _Optional[_Iterable[_Union[Error, _Mapping]]] = ...) -> None: ...

class PickTasksRequest(_message.Message):
    __slots__ = ("n", "group_size", "strategy", "family", "seed", "new_epoch")
    N_FIELD_NUMBER: _ClassVar[int]
    GROUP_SIZE_FIELD_NUMBER: _ClassVar[int]
    STRATEGY_FIELD_NUMBER: _ClassVar[int]
    FAMILY_FIELD_NUMBER: _ClassVar[int]
    SEED_FIELD_NUMBER: _ClassVar[int]
    NEW_EPOCH_FIELD_NUMBER: _ClassVar[int]
    n: int
    group_size: int
    strategy: str
    family: str
    seed: int
    new_epoch: bool
    def __init__(self, n: _Optional[int] = ..., group_size: _Optional[int] = ..., strategy: _Optional[str] = ..., family: _Optional[str] = ..., seed: _Optional[int] = ..., new_epoch: _Optional[bool] = ...) -> None: ...

class TaskPick(_message.Message):
    __slots__ = ("task_id", "task_dir", "pass_estimate", "usefulness")
    TASK_ID_FIELD_NUMBER: _ClassVar[int]
    TASK_DIR_FIELD_NUMBER: _ClassVar[int]
    PASS_ESTIMATE_FIELD_NUMBER: _ClassVar[int]
    USEFULNESS_FIELD_NUMBER: _ClassVar[int]
    task_id: str
    task_dir: str
    pass_estimate: float
    usefulness: float
    def __init__(self, task_id: _Optional[str] = ..., task_dir: _Optional[str] = ..., pass_estimate: _Optional[float] = ..., usefulness: _Optional[float] = ...) -> None: ...

class PickTasksResult(_message.Message):
    __slots__ = ("tasks",)
    TASKS_FIELD_NUMBER: _ClassVar[int]
    tasks: _containers.RepeatedCompositeFieldContainer[TaskPick]
    def __init__(self, tasks: _Optional[_Iterable[_Union[TaskPick, _Mapping]]] = ...) -> None: ...

class GcRequest(_message.Message):
    __slots__ = ("grace_seconds", "budget_bytes")
    GRACE_SECONDS_FIELD_NUMBER: _ClassVar[int]
    BUDGET_BYTES_FIELD_NUMBER: _ClassVar[int]
    grace_seconds: int
    budget_bytes: int
    def __init__(self, grace_seconds: _Optional[int] = ..., budget_bytes: _Optional[int] = ...) -> None: ...

class GcResult(_message.Message):
    __slots__ = ("snapshots_removed", "layers_removed", "orphans_removed", "bytes_freed", "bytes_total")
    SNAPSHOTS_REMOVED_FIELD_NUMBER: _ClassVar[int]
    LAYERS_REMOVED_FIELD_NUMBER: _ClassVar[int]
    ORPHANS_REMOVED_FIELD_NUMBER: _ClassVar[int]
    BYTES_FREED_FIELD_NUMBER: _ClassVar[int]
    BYTES_TOTAL_FIELD_NUMBER: _ClassVar[int]
    snapshots_removed: int
    layers_removed: int
    orphans_removed: int
    bytes_freed: int
    bytes_total: int
    def __init__(self, snapshots_removed: _Optional[int] = ..., layers_removed: _Optional[int] = ..., orphans_removed: _Optional[int] = ..., bytes_freed: _Optional[int] = ..., bytes_total: _Optional[int] = ...) -> None: ...

class StatsRequest(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...

class Stats(_message.Message):
    __slots__ = ("values", "prometheus", "level", "version")
    class ValuesEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: float
        def __init__(self, key: _Optional[str] = ..., value: _Optional[float] = ...) -> None: ...
    VALUES_FIELD_NUMBER: _ClassVar[int]
    PROMETHEUS_FIELD_NUMBER: _ClassVar[int]
    LEVEL_FIELD_NUMBER: _ClassVar[int]
    VERSION_FIELD_NUMBER: _ClassVar[int]
    values: _containers.ScalarMap[str, float]
    prometheus: str
    level: str
    version: str
    def __init__(self, values: _Optional[_Mapping[str, float]] = ..., prometheus: _Optional[str] = ..., level: _Optional[str] = ..., version: _Optional[str] = ...) -> None: ...

class DrainRequest(_message.Message):
    __slots__ = ("grace",)
    GRACE_FIELD_NUMBER: _ClassVar[int]
    grace: _duration_pb2.Duration
    def __init__(self, grace: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ...) -> None: ...
