//
// Copyright (c) 2020-2021 science+computing ag and other contributors
//
// This program and the accompanying materials are made
// available under the terms of the Eclipse Public License 2.0
// which is available at https://www.eclipse.org/legal/epl-2.0/
//
// SPDX-License-Identifier: EPL-2.0
//

//! Module containing utilities for the filestore implementation
//!

use std::collections::BTreeMap;

use anyhow::anyhow;
use anyhow::Result;
use indicatif::ProgressBar;
use resiter::AndThen;

use crate::filestore::path::*;
use crate::filestore::Artifact;

/// The actual filestore implementation
///
/// Because the "staging" filestore and the "release" filestore function the same underneath, we
/// provide this type as the implementation.
///
/// It can then be wrapped into the actual interface of this module with specialized functionality.
pub struct FileStoreImpl {
    pub(in crate::filestore) root: StoreRoot,
    store: BTreeMap<ArtifactPath, Artifact>,
}

impl FileStoreImpl {
    /// Loads the passed path recursively into a Path => Artifact mapping
    pub fn load(root: StoreRoot, progress: ProgressBar) -> Result<Self> {
        let store = root
            .find_artifacts_recursive()
            .and_then_ok(|artifact_path| {
                progress.tick();
                Artifact::load(&root, artifact_path.clone()).map(|a| (artifact_path, a))
            })
            .collect::<Result<BTreeMap<ArtifactPath, Artifact>>>()?;

        Ok(FileStoreImpl { root, store })
    }

    pub fn root_path(&self) -> &StoreRoot {
        &self.root
    }

    pub fn get(&self, artifact_path: &ArtifactPath) -> Option<&Artifact> {
        self.store.get(artifact_path)
    }

    pub(in crate::filestore) fn load_from_path(
        &mut self,
        artifact_path: &ArtifactPath,
    ) -> Result<&Artifact> {
        if self.store.get(&artifact_path).is_some() {
            Err(anyhow!("Entry exists: {}", artifact_path.display()))
        } else {
            Ok(self
                .store
                .entry(artifact_path.clone())
                .or_insert(Artifact::load(&self.root, artifact_path.clone())?))
        }
    }
}
