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

use std::io::Result;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use bytes::Buf;
use quick_xml::de;
use quick_xml::escape::unescape;
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::backend::Backend;
use super::error::parse_error;
use crate::error::new_other_object_error;
use crate::http_util::parse_error_response;
use crate::object::ObjectPageStream;
use crate::ops::Operation;
use crate::path::build_rel_path;
use crate::ObjectEntry;
use crate::ObjectMetadata;
use crate::ObjectMode;

pub struct DirStream {
    backend: Arc<Backend>,
    root: String,
    path: String,

    token: Option<String>,

    done: bool,
}

impl DirStream {
    pub fn new(backend: Arc<Backend>, root: &str, path: &str) -> Self {
        Self {
            backend,
            root: root.to_string(),
            path: path.to_string(),

            token: None,

            done: false,
        }
    }
}

#[async_trait]
impl ObjectPageStream for DirStream {
    async fn next_page(&mut self) -> Result<Option<Vec<ObjectEntry>>> {
        if self.done {
            return Ok(None);
        }

        let resp = self
            .backend
            .oss_list_object(&self.path, self.token.clone())
            .await?;

        if resp.status() != http::StatusCode::OK {
            let er = parse_error_response(resp).await?;
            let err = parse_error(Operation::List, &self.path, er);
            return Err(err);
        }

        let bs = resp.into_body().bytes().await.map_err(|e| {
            new_other_object_error(Operation::List, &self.path, anyhow!("read body: {:?}", e))
        })?;

        let output: ListBucketOutput = de::from_reader(bs.reader()).map_err(|e| {
            new_other_object_error(
                Operation::List,
                &self.path,
                anyhow!("deserialize list_bucket output: {:?}", e),
            )
        })?;

        self.done = !output.is_truncated;
        self.token = output.next_continuation_token.clone();

        let mut entries = Vec::with_capacity(output.common_prefixes.len() + output.contents.len());

        for prefix in output.common_prefixes {
            let de = ObjectEntry::new(
                self.backend.clone(),
                &build_rel_path(&self.root, &prefix.prefix),
                ObjectMetadata::new(ObjectMode::DIR),
            )
            .with_complete();
            entries.push(de);
        }

        for object in output.contents {
            if object.key.ends_with('/') {
                continue;
            }
            let mut meta = ObjectMetadata::new(ObjectMode::FILE);

            meta.set_etag(&object.etag);
            meta.set_content_length(object.size);
            let dt = OffsetDateTime::parse(object.last_modified.as_str(), &Rfc3339)
                .map(|v| {
                    v.replace_nanosecond(0)
                        .expect("replace nanosecond of last modified must succeed")
                })
                .map_err(|e| {
                    new_other_object_error(
                        Operation::List,
                        &self.path,
                        anyhow!("parse last modified RFC3339 datetime: {e:?}"),
                    )
                })?;
            meta.set_last_modified(dt);

            let rel = build_rel_path(&self.root, &object.key);
            let path = unescape(&rel)
                .map_err(|e| new_other_object_error(Operation::List, &self.path, e))?;
            let de = ObjectEntry::new(self.backend.clone(), &path, meta);
            entries.push(de);
        }

        Ok(Some(entries))
    }
}

#[derive(Default, Debug, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct ListBucketOutput {
    prefix: String,
    max_keys: u64,
    encoding_type: String,
    is_truncated: bool,
    common_prefixes: Vec<CommonPrefix>,
    contents: Vec<Content>,
    key_count: u64,

    next_continuation_token: Option<String>,
}

#[derive(Default, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "PascalCase")]
struct Content {
    key: String,
    last_modified: String,
    #[serde(rename = "ETag")]
    etag: String,
    size: u64,
}

#[derive(Default, Debug, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct CommonPrefix {
    prefix: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_list_output() {
        let bs = bytes::Bytes::from(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="https://doc.oss-cn-hangzhou.aliyuncs.com">
    <Name>examplebucket</Name>
    <Prefix></Prefix>
    <StartAfter>b</StartAfter>
    <MaxKeys>3</MaxKeys>
    <EncodingType>url</EncodingType>
    <IsTruncated>true</IsTruncated>
    <NextContinuationToken>CgJiYw--</NextContinuationToken>
    <Contents>
        <Key>b/c</Key>
        <LastModified>2020-05-18T05:45:54.000Z</LastModified>
        <ETag>"35A27C2B9EAEEB6F48FD7FB5861D****"</ETag>
        <Size>25</Size>
        <StorageClass>STANDARD</StorageClass>
        <Owner>
            <ID>1686240967192623</ID>
            <DisplayName>1686240967192623</DisplayName>
        </Owner>
    </Contents>
    <Contents>
        <Key>ba</Key>
        <LastModified>2020-05-18T11:17:58.000Z</LastModified>
        <ETag>"35A27C2B9EAEEB6F48FD7FB5861D****"</ETag>
        <Size>25</Size>
        <StorageClass>STANDARD</StorageClass>
        <Owner>
            <ID>1686240967192623</ID>
            <DisplayName>1686240967192623</DisplayName>
        </Owner>
    </Contents>
    <Contents>
        <Key>bc</Key>
        <LastModified>2020-05-18T05:45:59.000Z</LastModified>
        <ETag>"35A27C2B9EAEEB6F48FD7FB5861D****"</ETag>
        <Size>25</Size>
        <StorageClass>STANDARD</StorageClass>
        <Owner>
            <ID>1686240967192623</ID>
            <DisplayName>1686240967192623</DisplayName>
        </Owner>
    </Contents>
    <KeyCount>3</KeyCount>
</ListBucketResult>"#,
        );

        let out: ListBucketOutput = de::from_reader(bs.reader()).expect("must_success");

        assert!(out.is_truncated);
        assert_eq!(out.next_continuation_token, Some("CgJiYw--".to_string()));
        assert!(out.common_prefixes.is_empty());

        assert_eq!(
            out.contents,
            vec![
                Content {
                    key: "b/c".to_string(),
                    last_modified: "2020-05-18T05:45:54.000Z".to_string(),
                    etag: "\"35A27C2B9EAEEB6F48FD7FB5861D****\"".to_string(),
                    size: 25,
                },
                Content {
                    key: "ba".to_string(),
                    last_modified: "2020-05-18T11:17:58.000Z".to_string(),
                    etag: "\"35A27C2B9EAEEB6F48FD7FB5861D****\"".to_string(),
                    size: 25,
                },
                Content {
                    key: "bc".to_string(),
                    last_modified: "2020-05-18T05:45:59.000Z".to_string(),
                    etag: "\"35A27C2B9EAEEB6F48FD7FB5861D****\"".to_string(),
                    size: 25,
                }
            ]
        )
    }
}
