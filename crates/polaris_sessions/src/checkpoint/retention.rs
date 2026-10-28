//! Retention policy and usage reporting for one session's checkpoint set.

/// Bounds on one session's retained checkpoint set.
///
/// Both bounds apply **per session**, never per process: a global budget would
/// let one busy session evict another session's recovery points, with no way
/// for either consumer to diagnose it.
///
/// `None` on an axis means unbounded on that axis. That is the pre-retention
/// behaviour and is never the default.
///
/// `Some(0)` is valid and deliberately means "retain no checkpoints": every
/// capture is refused with
/// [`SessionError::CheckpointCapacity`](crate::SessionError::CheckpointCapacity).
/// Hosts that merely want to stop automatic capture should instead use
/// [`SessionsPlugin::without_auto_checkpoint`](crate::SessionsPlugin::without_auto_checkpoint),
/// which still permits explicit captures.
///
/// # What is bounded, and what is not
///
/// A session store can measure two things about a checkpoint without learning
/// any consumer vocabulary: how many there are, and how many bytes they
/// serialize to. Those are the two bounds. Nothing here knows about files,
/// paths, workspaces, edits, images or tool invocations — a consumer that
/// budgets those owns its own limits, and reads the framework's with
/// [`SessionCheckpoints::retention`](super::SessionCheckpoints::retention)
/// rather than restating them.
///
/// # Defaults
///
/// [`Default`] is 64 checkpoints and 16 MiB. **These are initial product
/// bounds, not measured performance claims.** They are injected policy: a host
/// overrides them with
/// [`SessionsPlugin::with_checkpoint_retention`](crate::SessionsPlugin::with_checkpoint_retention)
/// and no call site reads a constant.
///
/// # Examples
///
/// ```
/// use polaris_sessions::checkpoint::CheckpointRetention;
///
/// let default = CheckpointRetention::default();
/// assert_eq!(default.max_count, Some(64));
///
/// // A host that keeps fewer, smaller points:
/// let tight = CheckpointRetention::new(Some(8), Some(256 * 1024));
/// assert_eq!(tight.max_count, Some(8));
///
/// // Opting out of both bounds is explicit.
/// assert_eq!(CheckpointRetention::unbounded().max_bytes, None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckpointRetention {
    /// Maximum number of checkpoints one session retains.
    pub max_count: Option<usize>,
    /// Maximum total serialized payload bytes across one session's retained
    /// checkpoints.
    pub max_bytes: Option<usize>,
}

/// Default retained checkpoint count. An initial product bound, not a measured
/// performance claim.
const DEFAULT_MAX_COUNT: usize = 64;

/// Default retained checkpoint payload budget. An initial product bound, not a
/// measured performance claim.
const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;

impl CheckpointRetention {
    /// Creates a policy from explicit bounds.
    ///
    /// A zero bound retains no checkpoints and makes every capture refuse.
    #[must_use]
    pub fn new(max_count: Option<usize>, max_bytes: Option<usize>) -> Self {
        Self {
            max_count,
            max_bytes,
        }
    }

    /// Creates a policy that bounds neither count nor bytes.
    ///
    /// Restores the pre-retention behaviour, including its unbounded growth.
    ///
    /// A `FileStore` still refuses to save a session past its
    /// `FileStoreLimits`; raise them with `FileStore::with_load_limits` if an
    /// unbounded session can grow that large.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::new(None, None)
    }
}

impl Default for CheckpointRetention {
    fn default() -> Self {
        Self::new(Some(DEFAULT_MAX_COUNT), Some(DEFAULT_MAX_BYTES))
    }
}

/// What one session is currently using against its [`CheckpointRetention`].
///
/// `count` and `bytes` are measured on the retained set, never estimated, so
/// neither can under-report a budget the caller is about to spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckpointUsage {
    /// Retained checkpoints.
    pub count: usize,
    /// Total serialized payload bytes across the retained checkpoints.
    pub bytes: usize,
    /// The configured count bound, or `None` when unbounded.
    pub max_count: Option<usize>,
    /// The configured byte bound, or `None` when unbounded.
    pub max_bytes: Option<usize>,
    /// Retained checkpoints that cannot currently be evicted: those held by a
    /// [`CheckpointPin`](super::CheckpointPin), the session's current
    /// checkpoint, and the two endpoints of a restore that has not completed.
    pub pinned: usize,
}
