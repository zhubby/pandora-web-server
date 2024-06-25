// Copyright 2024 Wladimir Palant
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Structures required to deserialize Headers Module configuration from YAML configuration files.

// https://github.com/rust-lang/rust-clippy/issues/9776
#![allow(clippy::mutable_key_type)]

use http::{
    header,
    header::{HeaderName, HeaderValue},
};
use module_utils::merger::{HostPathMatcher, PathMatch, PathMatchResult};
use module_utils::router::{Path, EMPTY_PATH};
use module_utils::{DeserializeMap, OneOrMany};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Debug;

/// Include and exclude rules applying to a configuration entry
///
/// When deciding which rule applies, the “closest” rule to the host/path combination is selected:
///
/// * If a rule like `example.com/dir` applies to this exact host/path combination, that rule is
///   selected.
/// * If a prefix rule like `example.com/dir/*` applies to this host/path combination, it applies
///   if all similar rules match a shorter path.
/// * Fallback rules like `/dir/*` apply only if no host-specific rule matches the host/path
///   combination. When multiple matching fallback rules exist, one is selected using the criteria
///   above.
///
/// The configuration entry is only applied to a host/path configuration if there is a matching
/// rule and that rule is an include rule.
#[derive(Debug, Default, Clone, PartialEq, Eq, DeserializeMap)]
pub struct MatchRules {
    /// Rules determining the locations where the configuration entry should apply
    pub include: OneOrMany<HostPathMatcher>,
    /// Rules determining the locations where the configuration entry should not apply
    pub exclude: OneOrMany<HostPathMatcher>,
}

impl PathMatch for MatchRules {
    fn iter(&self) -> Box<dyn Iterator<Item = (&[u8], &Path)> + '_> {
        if self.include.is_empty() && self.exclude.is_empty() {
            Box::new(std::iter::once(("".as_bytes(), EMPTY_PATH)))
        } else {
            Box::new(
                self.include
                    .iter()
                    .chain(self.exclude.iter())
                    .flat_map(|matcher| matcher.iter()),
            )
        }
    }

    fn matches(&self, host: &[u8], path: &Path, force_prefix: bool) -> PathMatchResult {
        fn find_match<'a>(
            rules: &'a [HostPathMatcher],
            host: &[u8],
            path: &Path,
            force_prefix: bool,
        ) -> (PathMatchResult, Option<&'a HostPathMatcher>) {
            rules.iter().fold(
                (PathMatchResult::EMPTY, None),
                |(previous_result, previous), current| {
                    let result = current.matches(host, path, force_prefix);
                    if result.any() {
                        if previous.is_some_and(|previous| previous > current) {
                            (previous_result, previous)
                        } else {
                            (result, Some(current))
                        }
                    } else {
                        (previous_result, previous)
                    }
                },
            )
        }

        if self.include.is_empty() && self.exclude.is_empty() {
            // By default, this is a fallback rule matching everything
            let result = if host.is_empty() {
                PathMatchResult::EMPTY
            } else {
                PathMatchResult::EMPTY.set_fallback()
            };

            return if path.is_empty() {
                result.set_exact().set_prefix()
            } else {
                result.set_prefix()
            };
        }

        let (_, exclude) = find_match(&self.exclude, host, path, force_prefix);
        let (include_result, include) = find_match(&self.include, host, path, force_prefix);
        if let Some(exclude) = exclude {
            if include.is_some_and(|include| include > exclude) {
                include_result
            } else {
                PathMatchResult::EMPTY
            }
        } else {
            include_result
        }
    }
}

pub(crate) type Header = (HeaderName, HeaderValue);

pub(crate) trait IntoHeaders {
    /// Merges two configurations, with conflicting settings from `other` being prioritized.
    fn merge_with(&mut self, other: &Self);

    /// Translates the configuration into a list of HTTP headers.
    fn into_headers(self) -> Vec<Header>;
}

/// Combines a given configuration with match rules determining what host/path combinations it
/// should apply to.
#[derive(Debug, Default, Clone, PartialEq, Eq, DeserializeMap)]
pub struct WithMatchRules<C: Default + Clone + PartialEq + Eq> {
    /// The match rules
    #[module_utils(flatten)]
    pub match_rules: MatchRules,

    /// The actual configuration
    #[module_utils(flatten)]
    pub conf: C,
}

macro_rules! impl_conf {
    (
        $variant:tt:
        $(#[$attr:meta])*
        $vis:vis struct $struct_name:ident
        {
            $($name:ident($header_name:literal, $($type:tt)+),)*
        }
    ) => {
        $(#[$attr])*
        #[derive(Debug, Default, Clone, PartialEq, Eq, DeserializeMap)]
        $vis struct $struct_name {
            $(
                #[doc = impl_conf!(doc($header_name, $variant $($type)+))]
                #[module_utils(rename = $header_name)]
                pub $name: $($type)+,
            )*
        }

        impl IntoHeaders for $struct_name {
            fn merge_with(&mut self, other: &Self) {
                $(
                    impl_conf!(merge(self.$name, other.$name, $($type)+));
                )*
            }
            fn into_headers(self) -> Vec<Header> {
                let mut entries: Vec<Cow<'_, str>> = Vec::new();
                $(
                    impl_conf!(push(entries, $header_name, self.$name, $variant $($type)+));
                )*
                if entries.is_empty() {
                    Vec::new()
                } else {
                    impl_conf!(finalize(entries, $variant))
                }
            }
        }
    };

    // Merge is generic
    (merge($into:expr, $from:expr, bool)) => {
        if $from {
            $into = $from;
        }
    };
    (merge($into:expr, $from:expr, String)) => {
        if !$from.is_empty() {
            $into = $from.clone();
        }
    };
    (merge($into:expr, $from:expr, Option<$type:ty>)) => {
        if $from.is_some() {
            $into = $from;
        }
    };
    (merge($into:expr, $from:expr, Vec<$type:ty>)) => {
        $into.extend_from_slice(&$from);
    };

    // Cache-Control types
    (doc($header_name:literal, cache_control Option<usize>)) => {
        concat!("If set, ", $header_name, " option will be sent")
    };
    (doc($header_name:literal, cache_control bool)) => {
        concat!("If `true`, ", $header_name, " flag will be sent")
    };
    (push($list:expr, $header_name:literal, $value:expr, cache_control Option<usize>)) => {
        if let Some(value) = $value {
            $list.push(format!(concat!($header_name, "={}"), value).into());
        }
    };
    (push($list:expr, $header_name:literal, $value:expr, cache_control bool)) => {
        if $value {
            $list.push($header_name.into());
        }
    };
    (finalize($list:expr, cache_control)) => {
        vec![(
            header::CACHE_CONTROL,
            HeaderValue::from_str(&$list.join(", ")).unwrap(),
        )]
    };

    // Content-Security-Policy types
    (doc($header_name:literal, csp $($type:tt)*)) => {
        concat!("If set, ", $header_name, " directive will be sent")
    };
    (push($list:expr, $header_name:literal, $value:expr, csp bool)) => {
        if $value {
            $list.push($header_name.into());
        }
    };
    (push($list:expr, $header_name:literal, $value:expr, csp String)) => {
        if !$value.is_empty() {
            $list.push(format!(concat!($header_name, " {}"), $value).into());
        }
    };
    (push($list:expr, $header_name:literal, $value:expr, csp Vec<String>)) => {
        if !$value.is_empty() {
            $list.push(format!(concat!($header_name, " {}"), $value.join(" ")).into());
        }
    };
    (finalize($list:expr, csp)) => {
        vec![(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_str(&$list.join("; ")).unwrap(),
        )]
    };
}

impl_conf! {cache_control:
    /// Configuration for the Cache-Control header
    pub struct CacheControlConf {
        max_age("max-age", Option<usize>),
        s_maxage("s-maxage", Option<usize>),
        no_cache("no-cache", bool),
        no_storage("no-storage", bool),
        no_transform("no-transform", bool),
        must_revalidate("must-revalidate", bool),
        proxy_revalidate("proxy-revalidate", bool),
        must_understand("must-understand", bool),
        private("private", bool),
        public("public", bool),
        immutable("immutable", bool),
        stale_while_revalidate("stale-while-revalidate", Option<usize>),
        stale_if_error("stale-if-error", Option<usize>),
    }
}

impl_conf! {csp:
    /// Configuration for the Content-Security-Policy header
    pub struct ContentSecurityPolicyConf {
        connect_src("connect-src", Vec<String>),
        default_src("default-src", Vec<String>),
        fenced_frame_src("fenced-frame-src", Vec<String>),
        font_src("font-src", Vec<String>),
        frame_src("frame-src", Vec<String>),
        img_src("img-src", Vec<String>),
        manifest_src("manifest-src", Vec<String>),
        media_src("media-src", Vec<String>),
        object_src("object-src", Vec<String>),
        prefetch_src("prefetch-src", Vec<String>),
        script_src("script-src", Vec<String>),
        script_src_elem("script-src-elem", Vec<String>),
        script_src_attr("script-src-attr", Vec<String>),
        style_src("style-src", Vec<String>),
        style_src_elem("style-src-elem", Vec<String>),
        style_src_attr("style-src-attr", Vec<String>),
        worker_src("worker-src", Vec<String>),
        base_uri("base-uri", Vec<String>),
        sandbox("sandbox", Vec<String>),
        form_action("form-action", Vec<String>),
        frame_ancestors("frame-ancestors", Vec<String>),
        report_uri("report-uri", String),
        report_to("report-to", String),
        require_trusted_types_for("require-trusted-types-for", Vec<String>),
        trusted_types("trusted-types", Vec<String>),
        upgrade_insecure_requests("upgrade-insecure-requests", bool),
    }
}

/// Custom headers configuration
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CustomHeadersConf {
    pub(crate) headers: HashMap<HeaderName, HeaderValue>,
}

impl IntoHeaders for CustomHeadersConf {
    fn merge_with(&mut self, other: &Self) {
        self.headers.extend(
            other
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
    }

    fn into_headers(self) -> Vec<Header> {
        self.headers.into_iter().collect()
    }
}

/// Various settings to configure HTTP response headers
#[derive(Debug, Default, Clone, PartialEq, Eq, DeserializeMap)]
pub struct HeadersInnerConf {
    /// Cache-Control header
    pub cache_control: OneOrMany<WithMatchRules<CacheControlConf>>,

    /// Content-Security-Policy header
    pub content_security_policy: OneOrMany<WithMatchRules<ContentSecurityPolicyConf>>,

    /// Custom headers, headers configures as name => value map here
    pub custom: OneOrMany<WithMatchRules<CustomHeadersConf>>,
}

/// Configuration file settings of the headers module
#[derive(Debug, Default, Clone, PartialEq, Eq, DeserializeMap)]
pub struct HeadersConf {
    /// Various settings to configure HTTP response headers
    pub response_headers: HeadersInnerConf,
}
