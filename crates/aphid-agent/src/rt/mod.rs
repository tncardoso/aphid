//! A runtime for dynamic composition.
//!
//! A system built here is a tree of **components**. Each declares the services
//! it needs and the services it offers, and the runtime decides when it runs:
//! a component waits until everything it declared is available, loads, and
//! unloads again if any of it goes away. Nothing is ordered by hand.
//!
//! Everything a component registers is an **effect** that carries its inverse,
//! so unloading it removes its tools, its listeners, its services and its
//! children without any of them being tracked by the author.
//!
//! # The two halves
//!
//! *Temporal composability* is [`Disposer`]: a component can be removed and the
//! system returns to the state it would have been in had the component never
//! loaded.
//!
//! *Spatial composability* is [`Service`] and [`Realm`]: a component names
//! what it needs, the runtime resolves it, and the resolution can differ
//! between subtrees.
//!
//! # The system boundary
//!
//! An inverse only exists for what this process exclusively controls. That line
//! runs through the middle of most interesting operations, and it is worth
//! knowing where:
//!
//! - **Inside.** A registration, a binding, a listener, a mounted child, a
//!   spawned process recorded in [`exec::Registry`](crate::exec::Registry) —
//!   acquiring it installs a record here, and dropping the record is a true
//!   inverse.
//! - **Outside.** Bytes already written to a socket or a pipe, a request
//!   already sent, an append to a transcript that only ever grows. The
//!   acquisition is inside the boundary and the emission is not.
//!
//! For anything outside, a disposer can only **compensate** — delete the file
//! it created, refund the charge, kill the process it started. Compensations
//! compose in the same order inverses do, and the runtime treats them the same;
//! what it cannot do is promise they were equivalent.

mod bus;
mod component;
mod composition;
mod context;
mod effect;
mod fiber;
mod isolate;
mod loader;
mod reactor;
mod runtime;
pub mod schema;
mod service;
mod uid;

pub use bus::{
    Bailed, Bus, Emitted, Event, Failure, Next, Paralleled, Scope, Serialed, WaterfallFn,
    Waterfalled,
};
pub use component::Component;
pub use composition::Composition;
pub use context::Context;
pub use effect::{Disposer, Handle};
pub use fiber::{State, Status, Target};
pub use isolate::{Realm, Realms};
pub use loader::{Entry, Isolate, Loader, Report, Resolver};
pub use reactor::{Job, Reactor, Snapshot};
pub use runtime::Runtime;
pub use service::{Access, Binding, Service};
pub use uid::Uid;
