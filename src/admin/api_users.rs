// SPDX-License-Identifier: Apache-2.0
//! Admin panel API: user management endpoints (create/delete/disable,
//! password resets, per-table access grants). Split from the former
//! admin/mod.rs monolith — no behaviour change.

use super::*;

impl AdminPanel {
    pub(super) fn api_change_password(&self, session: &Session, req: &Request) -> Response {
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let old_pass = body["old_password"].as_str().unwrap_or("");
        let new_pass = body["new_password"].as_str().unwrap_or("");
        match self
            .security
            .users
            .change_password(&session.user, old_pass, new_pass)
        {
            Ok(()) => {
                self.revoke_user_tokens(&session.user);
                Response::json(200, &json!({"ok": true, "re_login": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    pub(super) fn api_list_users(&self, session: &Session) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        Response::json(200, &json!({"users": self.security.list_users()}))
    }

    pub(super) fn api_create_user(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        let pass = body["pass"].as_str().unwrap_or("");
        let role = body["role"].as_str().unwrap_or("viewer");
        let must_change = body["must_change"].as_bool().unwrap_or(true);
        let allowed_tables = match body.get("allowed_tables") {
            None | Some(J::Null) => None,
            Some(J::Array(values)) => Some(
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect(),
            ),
            Some(J::String(csv)) => match crate::access::normalize_csv(csv) {
                Ok(tables) => tables,
                Err(e) => return Response::json(400, &json!({"error": e})),
            },
            Some(_) => {
                return Response::json(
                    400,
                    &json!({"error": "allowed_tables must be an array of strings"}),
                );
            }
        };
        match self
            .security
            .users
            .create_user_with_tables(user, pass, role, must_change, allowed_tables)
        {
            Ok(()) => Response::json(201, &json!({"ok": true, "user": user, "role": role})),
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    pub(super) fn api_set_access(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        let allowed_tables = match body.get("allowed_tables") {
            None | Some(J::Null) => None,
            Some(J::Array(values)) => Some(
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect(),
            ),
            Some(J::String(csv)) => match crate::access::normalize_csv(csv) {
                Ok(tables) => tables,
                Err(e) => return Response::json(400, &json!({"error": e})),
            },
            Some(_) => {
                return Response::json(
                    400,
                    &json!({"error": "allowed_tables must be an array of strings"}),
                );
            }
        };
        match self.security.users.set_allowed_tables(user, allowed_tables) {
            Ok(()) => {
                self.revoke_user_tokens(user);
                Response::json(200, &json!({"ok": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    pub(super) fn api_reset_password(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        let new_pass = body["new_password"].as_str().unwrap_or("");
        match self.security.users.reset_password(user, new_pass) {
            Ok(()) => {
                self.revoke_user_tokens(user);
                Response::json(200, &json!({"ok": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    pub(super) fn api_disable_user(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        let disabled = body["disabled"].as_bool().unwrap_or(false);
        match self.security.users.set_disabled(user, disabled) {
            Ok(()) => {
                if disabled {
                    self.revoke_user_tokens(user);
                }
                Response::json(200, &json!({"ok": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    pub(super) fn api_delete_user(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        match self.security.users.delete_user(user) {
            Ok(()) => {
                self.revoke_user_tokens(user);
                Response::json(200, &json!({"ok": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }
}
