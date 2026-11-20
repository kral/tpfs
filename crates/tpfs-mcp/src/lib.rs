//! # tpfs-mcp
//!
//! A Model Context Protocol server (JSON-RPC over stdio) that gives AI agents *safe hands* for files.
//! An agent can look around and **propose** a plan; the plan is validated, assessed and stored as
//! pending. It runs only after a **human approves** it — by default with `tpfs approve <id>` in a
//! terminal, a channel the agent does not control. The approval is bound to the plan's digest.

mod recipes;
mod server;
mod store;

pub use recipes::{Installed, InstalledRecipes, MAX_PENDING_RECIPES, PendingRecipe, RecipeStatus, RecipeStore};
pub use server::{Approval, Config, MAX_MESSAGE, PROTOCOL_VERSION, Server};
pub use store::{MAX_PENDING, Pending, Store};
