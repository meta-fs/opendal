// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::fmt::Debug;
use std::io::ErrorKind;
use std::io::Result;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use futures::AsyncReadExt;
use log::debug;
use time::OffsetDateTime;

use super::dir_stream::DirStream;
use super::error::parse_io_error;
use crate::accessor::AccessorCapability;
use crate::error::new_other_backend_error;
use crate::error::new_other_object_error;
use crate::object::EmptyObjectStreamer;
use crate::ops::OpCreate;
use crate::ops::OpDelete;
use crate::ops::OpList;
use crate::ops::OpRead;
use crate::ops::OpStat;
use crate::ops::OpWrite;
use crate::ops::Operation;
use crate::path::build_rooted_abs_path;
use crate::path::normalize_root;
use crate::Accessor;
use crate::AccessorMetadata;
use crate::BytesReader;
use crate::ObjectMetadata;
use crate::ObjectMode;
use crate::ObjectStreamer;
use crate::Scheme;

/// Builder for hdfs services
#[derive(Debug, Default)]
pub struct Builder {
    root: Option<String>,
    name_node: Option<String>,
}

impl Builder {
    pub(crate) fn from_iter(it: impl Iterator<Item = (String, String)>) -> Self {
        let mut builder = Builder::default();

        for (k, v) in it {
            let v = v.as_str();
            match k.as_ref() {
                "root" => builder.root(v),
                "name_node" => builder.name_node(v),
                _ => continue,
            };
        }

        builder
    }

    /// Set root of this backend.
    ///
    /// All operations will happen under this root.
    pub fn root(&mut self, root: &str) -> &mut Self {
        self.root = if root.is_empty() {
            None
        } else {
            Some(root.to_string())
        };

        self
    }

    /// Set name_node of this backend.
    ///
    /// Vaild format including:
    ///
    /// - `default`: using the default setting based on hadoop config.
    /// - `hdfs://127.0.0.1:9000`: connect to hdfs cluster.
    pub fn name_node(&mut self, name_node: &str) -> &mut Self {
        if !name_node.is_empty() {
            // Trim trailing `/` so that we can accept `http://127.0.0.1:9000/`
            self.name_node = Some(name_node.trim_end_matches('/').to_string())
        }

        self
    }

    /// Finish the building and create hdfs backend.
    pub fn build(&mut self) -> Result<impl Accessor> {
        debug!("backend build started: {:?}", &self);

        let name_node = match &self.name_node {
            None => {
                return Err(new_other_backend_error(
                    HashMap::new(),
                    anyhow!("endpoint must be specified"),
                ))
            }
            Some(v) => v,
        };

        let root = normalize_root(&self.root.take().unwrap_or_default());
        debug!("backend use root {}", root);

        let client = hdrs::Client::connect(name_node).map_err(|e| {
            new_other_backend_error(
                HashMap::from([
                    ("root".to_string(), root.clone()),
                    ("endpoint".to_string(), name_node.clone()),
                ]),
                anyhow!("connect hdfs name node: {}", e),
            )
        })?;

        // Create root dir if not exist.
        if let Err(e) = client.metadata(&root) {
            if e.kind() == ErrorKind::NotFound {
                debug!("root {} is not exist, creating now", root);

                client.create_dir(&root).map_err(|e| {
                    new_other_backend_error(
                        HashMap::from([
                            ("root".to_string(), root.clone()),
                            ("endpoint".to_string(), name_node.clone()),
                        ]),
                        anyhow!("create root dir: {}", e),
                    )
                })?
            }
        }

        debug!("backend build finished: {:?}", &self);
        Ok(Backend {
            root,
            client: Arc::new(client),
        })
    }
}

/// Backend for hdfs services.
#[derive(Debug, Clone)]
pub struct Backend {
    root: String,
    client: Arc<hdrs::Client>,
}

/// hdrs::Client is thread-safe.
unsafe impl Send for Backend {}
unsafe impl Sync for Backend {}

#[async_trait]
impl Accessor for Backend {
    fn metadata(&self) -> AccessorMetadata {
        let mut am = AccessorMetadata::default();
        am.set_scheme(Scheme::Hdfs)
            .set_root(&self.root)
            .set_capabilities(
                AccessorCapability::Read | AccessorCapability::Write | AccessorCapability::List,
            );

        am
    }

    async fn create(&self, path: &str, args: OpCreate) -> Result<()> {
        let p = build_rooted_abs_path(&self.root, path);

        match args.mode() {
            ObjectMode::FILE => {
                let parent = PathBuf::from(&p)
                    .parent()
                    .ok_or_else(|| {
                        new_other_object_error(
                            Operation::Create,
                            path,
                            anyhow!("malformed path: {:?}", path),
                        )
                    })?
                    .to_path_buf();

                self.client
                    .create_dir(&parent.to_string_lossy())
                    .map_err(|e| parse_io_error(e, Operation::Create, &parent.to_string_lossy()))?;

                self.client
                    .open_file()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&p)
                    .map_err(|e| parse_io_error(e, Operation::Create, path))?;

                Ok(())
            }
            ObjectMode::DIR => {
                self.client
                    .create_dir(&p)
                    .map_err(|e| parse_io_error(e, Operation::Create, path))?;

                Ok(())
            }
            ObjectMode::Unknown => unreachable!(),
        }
    }

    async fn read(&self, path: &str, args: OpRead) -> Result<BytesReader> {
        let p = build_rooted_abs_path(&self.root, path);

        let mut f = self.client.open_file().read(true).open(&p)?;

        if let Some(offset) = args.offset() {
            f.seek(SeekFrom::Start(offset))
                .map_err(|e| parse_io_error(e, Operation::Read, path))?;
        };

        let f: BytesReader = match args.size() {
            None => Box::new(f),
            Some(size) => Box::new(f.take(size)),
        };

        Ok(f)
    }

    async fn write(&self, path: &str, _: OpWrite, r: BytesReader) -> Result<u64> {
        let p = build_rooted_abs_path(&self.root, path);

        let parent = PathBuf::from(&p)
            .parent()
            .ok_or_else(|| {
                new_other_object_error(
                    Operation::Write,
                    path,
                    anyhow!("malformed path: {:?}", path),
                )
            })?
            .to_path_buf();

        self.client
            .create_dir(&parent.to_string_lossy())
            .map_err(|e| parse_io_error(e, Operation::Write, &parent.to_string_lossy()))?;

        let mut f = self.client.open_file().create(true).write(true).open(&p)?;

        let n = futures::io::copy(r, &mut f).await?;

        Ok(n)
    }

    async fn stat(&self, path: &str, _: OpStat) -> Result<ObjectMetadata> {
        let p = build_rooted_abs_path(&self.root, path);

        let meta = self
            .client
            .metadata(&p)
            .map_err(|e| parse_io_error(e, Operation::Stat, path))?;

        let mode = if meta.is_dir() {
            ObjectMode::DIR
        } else if meta.is_file() {
            ObjectMode::FILE
        } else {
            ObjectMode::Unknown
        };
        let mut m = ObjectMetadata::new(mode);
        m.set_content_length(meta.len());
        m.set_last_modified(OffsetDateTime::from(meta.modified()));

        Ok(m)
    }

    async fn delete(&self, path: &str, _: OpDelete) -> Result<()> {
        let p = build_rooted_abs_path(&self.root, path);

        let meta = self.client.metadata(&p);

        if let Err(err) = meta {
            return if err.kind() == ErrorKind::NotFound {
                Ok(())
            } else {
                Err(parse_io_error(err, Operation::Delete, path))
            };
        }

        // Safety: Err branch has been checked, it's OK to unwrap.
        let meta = meta.ok().unwrap();

        let result = if meta.is_dir() {
            self.client.remove_dir(&p)
        } else {
            self.client.remove_file(&p)
        };

        result.map_err(|e| parse_io_error(e, Operation::Delete, path))?;

        Ok(())
    }

    async fn list(&self, path: &str, _: OpList) -> Result<ObjectStreamer> {
        let p = build_rooted_abs_path(&self.root, path);

        let f = match self.client.read_dir(&p) {
            Ok(f) => f,
            Err(e) => {
                return if e.kind() == ErrorKind::NotFound {
                    Ok(Box::new(EmptyObjectStreamer))
                } else {
                    Err(parse_io_error(e, Operation::List, path))
                }
            }
        };

        let rd = DirStream::new(Arc::new(self.clone()), &self.root, f);

        Ok(Box::new(rd))
    }
}
