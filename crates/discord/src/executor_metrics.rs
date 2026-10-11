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
        (Get, [Some("guilds"), Some(_), Some("members"), Some(_), None, None]) => {
            "GET /guilds/:guild/members/:member"
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
        (Put, [Some("applications"), Some(_), Some("guilds"), Some(_), Some("commands"), None]) => {
            "PUT /applications/:application/guilds/:guild/commands"
        }
        (Post, [Some("interactions"), Some(_), Some(_), Some("callback"), None, None]) => {
            "POST /interactions/:interaction/:token/callback"
        }
        (Get, [Some("users"), Some("@me"), None, None, None, None]) => "GET /users/@me",
        (
            Patch,
            [Some("webhooks"), Some(_), Some(_), Some("messages"), Some("@original"), None],
        ) => "PATCH /webhooks/:application/:token/messages/@original",
        (Post, [Some("guilds"), Some(_), Some("channels"), None, None, None]) => {
            "POST /guilds/:guild/channels"
        }
        (Delete, [Some("channels"), Some(_), None, None, None, None]) => {
            "DELETE /channels/:channel"
        }
        _ => "other",
    }
}

/// Cancellation (including the outer moderation timeout) is a transport failure,
/// not a lost attempt. Each send records exactly one completion or cancellation.
pub(crate) struct Attempt<'a> {
    metrics: &'a metrics::Metrics,
    route: &'static str,
    complete: bool,
}

impl Attempt<'static> {
    pub(crate) fn new(request: &Request) -> Self {
        Self {
            metrics: metrics::global(),
            route: route(request),
            complete: false,
        }
    }
}

impl Attempt<'_> {
    pub(crate) fn finish(&mut self, status: Option<u16>) {
        self.metrics.rest_response(self.route, status);
        self.complete = true;
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.metrics.rest_response(self.route, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_http::request::RequestBuilder;
    use twilight_model::id::{
        marker::{ApplicationMarker, GuildMarker},
        Id,
    };

    #[tokio::test]
    async fn cancellation_and_headers_each_count_exactly_one_send() {
        let metrics = metrics::Metrics::default();
        let route = "GET /channels/:channel";
        let pending = async {
            let _attempt = Attempt {
                metrics: &metrics,
                route,
                complete: false,
            };
            std::future::pending::<()>().await;
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), pending)
                .await
                .is_err()
        );
        {
            let mut attempt = Attempt {
                metrics: &metrics,
                route,
                complete: false,
            };
            attempt.finish(Some(429));
            // Dropping after headers, even on body failure, must not count twice.
        }
        let text = metrics.render(None);
        assert!(text.contains("two_bot_rest_requests_total{route=\"GET /channels/:channel\",result=\"transport\"} 1\n"));
        assert!(text.contains(
            "two_bot_rest_requests_total{route=\"GET /channels/:channel\",result=\"429\"} 1\n"
        ));
    }

    #[tokio::test]
    async fn guild_command_publish_classifies_to_guild_template() {
        use twilight_http::request::TryIntoRequest;
        // Real twilight builder: the exact request publish_guild_commands sends.
        // Needs a Tokio context: Client::builder spawns the ratelimit actor.
        let client = twilight_http::Client::builder().build();
        let request = client
            .interaction(Id::<ApplicationMarker>::new(1))
            .set_guild_commands(Id::<GuildMarker>::new(2), &[])
            .try_into_request()
            .unwrap();
        assert_eq!(request.method(), Method::Put);
        assert_eq!(
            route(&request),
            "PUT /applications/:application/guilds/:guild/commands"
        );
        assert!(metrics::REST_ROUTES.contains(&route(&request)));
    }

    #[tokio::test]
    async fn member_self_and_interaction_edits_classify_to_allowlisted_templates() {
        use twilight_http::request::TryIntoRequest;
        use twilight_model::id::marker::UserMarker;
        // Real twilight builders: the exact requests member_role_ids,
        // current_bot_user_id and edit_interaction_response send. Needs a
        // Tokio context: Client::builder spawns the ratelimit actor.
        let client = twilight_http::Client::builder().build();
        for (guild, member) in [(100u64, 300u64), (987654321098765432, 123456789012345678)] {
            let request = client
                .guild_member(Id::<GuildMarker>::new(guild), Id::<UserMarker>::new(member))
                .try_into_request()
                .unwrap();
            assert_eq!(request.method(), Method::Get);
            assert_eq!(route(&request), "GET /guilds/:guild/members/:member");
            assert!(metrics::REST_ROUTES.contains(&route(&request)));
        }
        let request = client.current_user().try_into_request().unwrap();
        assert_eq!(request.method(), Method::Get);
        assert_eq!(route(&request), "GET /users/@me");
        assert!(metrics::REST_ROUTES.contains(&route(&request)));
        for token in ["tok-abc", "a-much-longer-interaction-token-0123456789"] {
            let request = client
                .interaction(Id::<ApplicationMarker>::new(7))
                .update_response(token)
                .content(Some("done"))
                .try_into_request()
                .unwrap();
            assert_eq!(request.method(), Method::Patch);
            assert_eq!(
                route(&request),
                "PATCH /webhooks/:application/:token/messages/@original"
            );
            assert!(metrics::REST_ROUTES.contains(&route(&request)));
        }
        // Snowflakes, tokens and query parameters never appear in labels.
        for (method, path, template) in [
            (Method::Get, "users/@me?limit=100", "GET /users/@me"),
            (
                Method::Get,
                "guilds/100/members/300?limit=1",
                "GET /guilds/:guild/members/:member",
            ),
            (
                Method::Patch,
                "webhooks/7/tok-abc/messages/@original?wait=true",
                "PATCH /webhooks/:application/:token/messages/@original",
            ),
        ] {
            let request = RequestBuilder::raw(method, path.to_owned())
                .build()
                .unwrap();
            assert_eq!(route(&request), template, "path: {path}");
        }
        // Unknown, wrong-method and extra/overlong shapes stay `other`: no
        // dynamic registry, and followup/DM-adjacent shapes stay out of scope.
        for (method, path) in [
            (Method::Get, "users/123"),
            (Method::Get, "users/@me/guilds"),
            (Method::Post, "guilds/100/members/300"),
            (Method::Get, "webhooks/7/tok-abc/messages/@original"),
            (Method::Patch, "webhooks/7/tok-abc/messages/123"),
            (Method::Patch, "webhooks/7/tok-abc/messages/@original/extra"),
            (Method::Patch, "webhooks/7/tok-abc/messages/@original/a/b"),
            (Method::Patch, "guilds/100/members"),
            (Method::Get, "guilds/100/members/300/roles/400"),
        ] {
            let request = RequestBuilder::raw(method, path.to_owned())
                .build()
                .unwrap();
            assert_eq!(route(&request), "other", "path: {path}");
        }
        // Representative 429/5xx counters render under the new templates.
        let metrics = metrics::Metrics::default();
        metrics.rest_response("GET /guilds/:guild/members/:member", Some(429));
        metrics.rest_response("GET /users/@me", Some(503));
        metrics.rest_response(
            "PATCH /webhooks/:application/:token/messages/@original",
            Some(429),
        );
        let text = metrics.render(None);
        assert!(text.contains(
            "two_bot_rest_requests_total{route=\"GET /guilds/:guild/members/:member\",result=\"429\"} 1\n"
        ));
        assert!(text
            .contains("two_bot_rest_requests_total{route=\"GET /users/@me\",result=\"5xx\"} 1\n"));
        assert!(text.contains(
            "two_bot_rest_requests_total{route=\"PATCH /webhooks/:application/:token/messages/@original\",result=\"429\"} 1\n"
        ));
    }

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

    #[tokio::test]
    async fn voice_lifecycle_writes_classify_to_allowlisted_templates() {
        use twilight_http::request::TryIntoRequest;
        use twilight_model::{
            channel::ChannelType,
            id::marker::{ChannelMarker, UserMarker},
        };
        // Real twilight builders: the exact requests RoomHttp sends for
        // create/move/delete. Needs a Tokio context: Client::builder spawns
        // the ratelimit actor.
        let client = twilight_http::Client::builder().build();
        let create = client
            .create_guild_channel(Id::<GuildMarker>::new(100), "room")
            .kind(ChannelType::GuildVoice)
            .try_into_request()
            .unwrap();
        assert_eq!(create.method(), Method::Post);
        assert_eq!(route(&create), "POST /guilds/:guild/channels");
        assert!(metrics::REST_ROUTES.contains(&route(&create)));
        let delete = client
            .delete_channel(Id::<ChannelMarker>::new(600))
            .try_into_request()
            .unwrap();
        assert_eq!(delete.method(), Method::Delete);
        assert_eq!(route(&delete), "DELETE /channels/:channel");
        assert!(metrics::REST_ROUTES.contains(&route(&delete)));
        let moved = client
            .update_guild_member(Id::<GuildMarker>::new(100), Id::<UserMarker>::new(300))
            .channel_id(Some(Id::<ChannelMarker>::new(600)))
            .try_into_request()
            .unwrap();
        assert_eq!(moved.method(), Method::Patch);
        assert_eq!(route(&moved), "PATCH /guilds/:guild/members/:member");
        assert!(metrics::REST_ROUTES.contains(&route(&moved)));
        // Over-long paths and unknown shapes collapse to `other`, never a new
        // series: cardinality stays fixed.
        let extra = RequestBuilder::raw(Method::Post, "guilds/100/channels/extra".to_owned())
            .build()
            .unwrap();
        assert_eq!(route(&extra), "other");
        assert!(metrics::REST_ROUTES.contains(&"other"));
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
            "guilds/1/channels",
            "applications/1/commands",
            "applications/1/guilds/2/commands",
            "interactions/1/secret/callback",
            "users/@me",
            "webhooks/1/secret/messages/@original",
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
