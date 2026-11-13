//! icloud-for-linux — mount iCloud Drive on Linux as a local-first FUSE filesystem.
//!
//! Copyright (C) 2026  einyx
//!
//! This program is free software: you can redistribute it and/or modify
//! it under the terms of the GNU General Public License as published by
//! the Free Software Foundation, either version 3 of the License, or
//! (at your option) any later version.
//!
//! This program is distributed in the hope that it will be useful,
//! but WITHOUT ANY WARRANTY; without even the implied warranty of
//! MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//! GNU General Public License for more details.

// The storage/sync/API surface is intentionally broader than what the CLI uses
// today; it is exercised as the iCloud protocol client lands.
#![allow(dead_code)]

pub mod auth;
pub mod blob;
pub mod clock;
pub mod config;
pub mod control;
pub mod db;
pub mod icloud;
pub mod icloud_api;
pub mod index;
pub mod journal;
pub mod service;
pub mod signal;
pub mod sync;
pub mod vfs;
pub mod working;
pub mod xattr;
