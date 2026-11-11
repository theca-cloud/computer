//! One step per version of the client state, from 0.2.0's (version 2) on.
//! A step is never edited once released (`scripts/ci.sh` refuses it): a fix
//! is a new step.

pub(super) mod v3;
pub(super) mod v4;
