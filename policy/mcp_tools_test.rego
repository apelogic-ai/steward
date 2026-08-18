package steward.mcp_test

allowed_input := {
	"principal": "alice@example.com",
	"service": "github",
	"tool": "search_repositories",
	"actionClass": "read",
	"scopes": ["repo"],
	"args": {},
	"tokenClaims": {
		"email": "alice@example.com",
		# HOP-1 subjects are immutable canonical IDs. The wrapper supplies the
		# verified email as the request principal and resolves the provider
		# connection separately by (issuer, subject).
		"sub": "usr_0123456789abcdef0123456789abcdef",
		"steward": {
			"acting_as": "user",
			"runtime_uid": "runtime-uid-a",
			"tools": [{
				"provider": "github",
				"resource": "search_repositories",
				"action": "read",
			}],
			"version": 3,
		},
	},
}

delegated_service_input := object.union(allowed_input, {
	"tokenClaims": object.union(allowed_input.tokenClaims, {
		"steward": object.union(allowed_input.tokenClaims.steward, {
			"acting_as": "service_for_user",
			"service": "steward-run",
		}),
	}),
})

pure_service_input := {
	"principal": "service:scheduled-scanner",
	"service": "github",
	"tool": "search_repositories",
	"actionClass": "read",
	"scopes": ["repo"],
	"args": {},
	"tokenClaims": {
		"email": "service:scheduled-scanner",
		"sub": "service:scheduled-scanner",
		"steward": {
			"acting_as": "service",
			"service": "scheduled-scanner",
			"runtime_uid": "runtime-uid-service-a",
			"tools": [{
				"provider": "github",
				"resource": "search_repositories",
				"action": "read",
			}],
			"version": 3,
		},
	},
}

test_runtime_tool_grant_allows_the_exact_tool if {
	data.steward.mcp.allow with input as allowed_input
}

test_delegated_service_preserves_user_and_service_attribution if {
	data.steward.mcp.allow with input as delegated_service_input
}

test_pure_service_uses_its_dedicated_subject if {
	data.steward.mcp.allow with input as pure_service_input
}

test_pure_service_cannot_resolve_a_user_principal if {
	request := object.union(pure_service_input, {"principal": "alice@example.com"})
	not data.steward.mcp.allow with input as request
}

test_service_claim_without_service_name_fails_closed if {
	authority := object.remove(delegated_service_input.tokenClaims.steward, {"service"})
	claims := object.union(object.remove(delegated_service_input.tokenClaims, {"steward"}), {"steward": authority})
	request := object.union(object.remove(delegated_service_input, {"tokenClaims"}), {"tokenClaims": claims})
	not data.steward.mcp.allow with input as request
}

test_gateway_contract_returns_an_allow_object if {
	data.steward.mcp.decision == {"allow": true} with input as allowed_input
}

test_runtime_tool_grant_rejects_a_different_acting_user if {
	claims := object.union(allowed_input.tokenClaims, {"email": "bob@example.org", "sub": "bob@example.org"})
	request := object.union(allowed_input, {"tokenClaims": claims})
	not data.steward.mcp.allow with input as request
}

test_gateway_contract_explains_an_email_mismatch if {
	claims := object.union(allowed_input.tokenClaims, {"email": "bob@example.org"})
	request := object.union(allowed_input, {"tokenClaims": claims})
	decision := data.steward.mcp.decision with input as request
	object.get(decision, "reason", "") == "verified token email does not match the request"
}

test_gateway_contract_explains_a_missing_email if {
	claims := object.remove(allowed_input.tokenClaims, {"email"})
	without_claims := object.remove(allowed_input, {"tokenClaims"})
	request := object.union(without_claims, {"tokenClaims": claims})
	decision := data.steward.mcp.decision with input as request
	object.get(decision, "reason", "") == "verified token email is unavailable"
}

test_gateway_contract_explains_a_missing_request_principal if {
	request := object.remove(allowed_input, {"principal"})
	decision := data.steward.mcp.decision with input as request
	object.get(decision, "reason", "") == "authenticated request principal is unavailable"
}

test_gateway_contract_accepts_a_canonical_subject_with_matching_email if {
	data.steward.mcp.decision == {"allow": true} with input as allowed_input
}

test_runtime_tool_grant_rejects_a_tool_outside_the_token if {
	request := object.union(allowed_input, {"tool": "create_issue", "actionClass": "write"})
	not data.steward.mcp.allow with input as request
}

test_gateway_contract_explains_a_tool_grant_mismatch if {
	request := object.union(allowed_input, {"tool": "create_issue", "actionClass": "write"})
	decision := data.steward.mcp.decision with input as request
	not decision.allow
	object.get(decision, "reason", "") == "verified token has no matching tool grant"
}
