//
// Copyright (c) 2020-2022 science+computing ag and other contributors
//
// This program and the accompanying materials are made
// available under the terms of the Eclipse Public License 2.0
// which is available at https://www.eclipse.org/legal/epl-2.0/
//
// SPDX-License-Identifier: EPL-2.0
//

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use getset::Getters;
use tracing::trace;
use serde::Deserialize;
use serde::Serialize;
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize, Getters)]
pub struct Source {
    #[getset(get = "pub")]
    url: Url,
    #[getset(get = "pub")]
    hash: SourceHash,
    #[getset(get = "pub")]
    download_manually: bool,
}

impl Source {
    #[cfg(test)]
    pub fn new(url: Url, hash: SourceHash) -> Self {
        Source {
            url,
            hash,
            download_manually: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Getters)]
pub struct SourceHash {
    #[serde(rename = "type")]
    #[getset(get = "pub")]
    hashtype: HashType,

    #[serde(rename = "hash")]
    #[getset(get = "pub")]
    value: HashValue,
}

impl SourceHash {
    pub async fn matches_hash_of<R: tokio::io::AsyncRead + Unpin>(&self, reader: R) -> Result<()> {
        trace!("Hashing buffer with: {:?}", self.hashtype);
        let h = self.hashtype
            .hash_from_reader(reader)
            .await
            .context("Hashing failed")?;
        trace!("Hashing buffer with: {} finished", self.hashtype);

        if h == self.value {
            trace!("Hash matches expected hash");
            Ok(())
        } else {
            trace!("Hash mismatch expected hash");
            Err(anyhow!(
                "Hash mismatch, expected '{}', got '{}'",
                self.value,
                h
            ))
        }
    }

    #[cfg(test)]
    pub fn new(hashtype: HashType, value: HashValue) -> Self {
        SourceHash { hashtype, value }
    }
}

#[derive(parse_display::Display, Clone, Debug, Serialize, Deserialize)]
pub enum HashType {
    #[serde(rename = "sha1")]
    #[display("sha1")]
    Sha1,

    #[serde(rename = "sha256")]
    #[display("sha256")]
    Sha256,

    #[serde(rename = "sha512")]
    #[display("sha512")]
    Sha512,
}

impl HashType {
    async fn hash_from_reader<R: tokio::io::AsyncRead + Unpin>(&self, mut reader: R) -> Result<HashValue> {
        use tokio::io::AsyncReadExt;

        let mut buffer = [0; 1024];

        match self {
            HashType::Sha1 => {
                use sha1::Digest;

                trace!("SHA1 hashing buffer");
                let mut m = sha1::Sha1::new();
                loop {
                    let count = reader.read(&mut buffer)
                        .await
                        .context("Reading buffer failed")?;

                    if count == 0 {
                        trace!("ready");
                        break;
                    }

                    m.update(&buffer[..count]);
                }
                Ok(HashValue(format!("{:x}", m.finalize())))
            }
            HashType::Sha256 => {
                use sha2::Digest;

                trace!("SHA256 hashing buffer");
                let mut m = sha2::Sha256::new();
                loop {
                    let count = reader.read(&mut buffer)
                        .await
                        .context("Reading buffer failed")?;

                    if count == 0 {
                        trace!("ready");
                        break;
                    }

                    m.update(&buffer[..count]);
                }
                let h = format!("{:x}", m.finalize());
                trace!("Hash = {:?}", h);
                Ok(HashValue(h))
            }
            HashType::Sha512 => {
                use sha2::Digest;

                trace!("SHA512 hashing buffer");
                let mut m = sha2::Sha512::new();
                loop {
                    let count = reader.read(&mut buffer)
                        .await
                        .context("Reading buffer failed")?;

                    if count == 0 {
                        trace!("ready");
                        break;
                    }

                    m.update(&buffer[..count]);
                }
                Ok(HashValue(String::from_utf8(m.finalize()[..].to_vec())?))
            }
        }
    }
}

#[derive(parse_display::Display, Serialize, Deserialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(transparent)]
#[display("{0}")]
pub struct HashValue(String);

#[cfg(test)]
impl From<String> for HashValue {
    fn from(s: String) -> Self {
        HashValue(s)
    }
}
