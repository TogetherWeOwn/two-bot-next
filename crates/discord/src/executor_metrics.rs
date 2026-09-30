//! Route templates only: never retain IDs, query strings or interaction tokens.

use twilight_http::request::{Method, Request};
use two_bot_core::metrics;

pub(crate) fn route(request: &Request) -> &'static str {
    let path = request.path().split('?').next().unwrap_or_default();
    // Fixed stack storage: even malformed/unbounded paths cannot grow the registry.
    let mut segments = path.trim_start_matches('/').split('/');
    let parts = [
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ];
    if segments.next().is_some() {
        return "other";
    }
    use Method::*;
    match (request.method(), parts) {
        (Get, [Some("channels"), Some(_), None, None, None, None]) => "GET /channels/:channel",
        (Patch, [Some("channels"), Some(_), None, None, None, None]) => "PATCH /channels/:channel",
        (Get, [Some("channels"), Some(_), Some("messages"), None, None, None]) => {
            "GET /channels/:channel/messages"
        }
        (Post, [Some("channels"), Some(_), Some("messages"), None, None, None]) => {
            "POST /channels/:channel/messages"
        }
        (Post, [Some("channels"), Some(_), Some("messages"), Some("bulk-delete"), None, None]) => {
            "POST /channels/:channel/messages/bulk-delete"
        }
        (Delete, [Some("channels"), Some(_), Some("messages"), Some(_), None, None]) => {
            "DELETE /channels/:channel/messages/:message"
        }
        (Put, [Some("channels"), Some(_), Some("permissions"), Some(_), None, None]) => {
            "PUT /channels/:channel/permissions/:overwrite"
        }
        (Delete, [Some("channels"), Some(_), Some("permissions"), Some(_), None, None]) => {
            "DELETE /channels/:channel/permissions/:overwrite"
        }
        (Get, [Some("guilds"), Some(_), None, None, None, None]) => "GET /guilds/:guild",
        (Get, [Some("guilds"), Some(_), Some("members"), None, None, None]) => {
            "GET /guilds/:guild/members"
        }
        (Get, [Some("guilds"), Some(_), Some("scheduled-events"), None, None, None]) => {
            "GET /guilds/:guild/scheduled-events"
        }
        (Put, [Some("guilds"), Some(_), Some("bans"), Some(_), None, None]) => {
            "PUT /guilds/:guild/bans/:member"
        }
        (Delete, [Some("guilds"), Some(_), Some("bans"), Some(_), None, None]) => {
            "DELETE /guilds/:guild/bans/:member"
        }
        (Delete, [Some("guilds"), Some(_), Some("members"), Some(_), None, None]) => {
            "DELETE /guilds/:guild/members/:member"
        }
        (Patch, [Some("guilds"), Some(_), Some("members"), Some(_), None, None]) => {
            "PATCH /guilds/:guild/members/:member"
        }
        (Put, [Some("guilds"), Some(_), Some("members"), Some(_), Some("roles"), Some(_)]) => {
            "PUT /guilds/:guild/members/:member/roles/:role"
        }
        (Delete, [Some("guilds"), Some(_), Some("members"), Some(_), Some("roles"), Some(_)]) => {
            "DELETE /guilds/:guild/members/:member/roles/:role"
        }
        (Post, [Some("guilds"), Some(_), Some("scheduled-events"), None, None, None]) => {
            "POST /guilds/:guild/scheduled-events"
        }
        (Patch, [Some("guilds"), Some(_), Some("scheduled-events"), Some(_), None, None]) => {
            "PATCH /guilds/:guild/scheduled-events/:event"
        }
        (Delete, [Some("guilds"), Some(_), Some("scheduled-events"), Some(_), None, None]) => {
            "DELETE /guilds/:guild/scheduled-events/:event"
        }
        (Put, [Some("applications"), Some(_), Some("commands"), None, None, None]) => {
            "PUT /applications/:application/commands"
        }
        (Post, [Some("interactions"), Some(_), Some(_), Some("callback"), None, None]) => {
            "POST /interactions/:interaction/:token/callback"
        }
        _ => "other",
    }
}

/// Cancellation (including the outer moderation timeout) is a transport failure,
/// not a lost attempt. Each send records exactly one completion or cancellation.
pub(crate) struct Attempt {
    route: &'static str,
    complete: bool,
}

impl Attempt {
    pub(crate) fn new(request: &Request) -> Self {
        Self {
            route: route(request),
            complete: false,
        }
    }

    pub(crate) fn finish(&mut self, status: Option<u16>) {
        metrics::global().rest_response(self.route, status);
        self.complete = true;
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.complete {
            metrics::global().rest_response(self.route, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_http::request::RequestBuilder;

    #[test]
    fn request_templates_drop_snowflakes_queries_and_tokens() {
        for id in 1..1000 {
            let request = RequestBuilder::raw(
                Method::Post,
                format!("interactions/{id}/secret-{id}/callback?private={id}"),
            )
            .build()
            .unwrap();
            assert_eq!(
                route(&request),
                "POST /interactions/:interaction/:token/callback"
            );
            let request =
                RequestBuilder::raw(Method::Get, format!("channels/{id}/messages?before={id}"))
                    .build()
                    .unwrap();
            assert_eq!(route(&request), "GET /channels/:channel/messages");
        }
        let request = RequestBuilder::raw(Method::Get, "unknown/secret/path".to_owned())
            .build()
            .unwrap();
        assert_eq!(route(&request), "other");
    }

    #[test]
    fn every_emitted_template_is_allowlisted() {
        let paths = [
            "channels/1",
            "channels/1/messages",
            "channels/1/messages/2",
            "channels/1/messages/bulk-delete",
            "channels/1/permissions/2",
            "guilds/1",
            "guilds/1/members",
            "guilds/1/members/2",
            "guilds/1/members/2/roles/3",
            "guilds/1/bans/2",
            "guilds/1/scheduled-events",
            "guilds/1/scheduled-events/2",
            "applications/1/commands",
            "interactions/1/secret/callback",
            "unknown",
        ];
        for path in paths {
            for method in [
                Method::Get,
                Method::Post,
                Method::Put,
                Method::Patch,
                Method::Delete,
            ] {
                let request = RequestBuilder::raw(method, path.to_owned())
                    .build()
                    .unwrap();
                assert!(metrics::REST_ROUTES.contains(&route(&request)));
            }
        }
    }
}
