//! Non-failing pre-run reports.

use crate::node::NodeId;
use std::any::TypeId;
use std::fmt;

/// An optional global read (`Option<Res<T>>`) that will be `None` for a
/// given context, because the server has no global `T`.
///
/// Returned by [`GraphExecutor::absent_optional_globals`]. This is not an
/// error: absence is allowed, and the system is expected to handle `None`.
/// The report exists so that a plugin someone forgot to register is visible
/// before a run, instead of silently turning a feature off.
///
/// Global resources cannot change once the first context exists, so an entry
/// here is a fact about the server setup, not a guess. The reverse does not
/// always hold: for a graph nested past the executor's recursion limit, an
/// empty report does not prove absence (see
/// [`GraphExecutor::absent_optional_globals`], "Depth limit").
///
/// Entries name the plugins and types the server is missing, so they are meant
/// for operators; keep them behind authentication if you expose them over HTTP.
///
/// [`GraphExecutor::absent_optional_globals`]: super::GraphExecutor::absent_optional_globals
///
/// # Examples
///
/// ```
/// use polaris_graph::{AbsentOptionalGlobal, Graph, GraphExecutor};
/// use polaris_system::param::{Res, SystemContext};
/// use polaris_system::resource::{GlobalResource, Resources};
/// use polaris_system::system;
/// use std::sync::Arc;
///
/// struct Approval;
/// impl GlobalResource for Approval {}
///
/// #[system]
/// async fn ask_human(approval: Option<Res<Approval>>) -> bool {
///     approval.is_some()
/// }
///
/// let mut graph = Graph::new();
/// graph.add_system(ask_human);
///
/// let ctx = SystemContext::with_globals(Arc::new(Resources::new()));
/// let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx);
///
/// assert_eq!(absent.len(), 1);
/// assert!(absent[0].resource_type.contains("Approval"));
/// assert!(absent[0].system_name.contains("ask_human"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsentOptionalGlobal {
    /// The system node that declares the optional read.
    pub node: NodeId,
    /// The name of the system that declares the optional read.
    pub system_name: &'static str,
    /// The name of the absent global resource type.
    pub resource_type: &'static str,
    /// The [`TypeId`] of the absent global resource type.
    pub type_id: TypeId,
}

impl fmt::Display for AbsentOptionalGlobal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "system '{}' ({}) reads optional global {} which the server does not have; it will be None",
            self.system_name, self.node, self.resource_type
        )
    }
}
