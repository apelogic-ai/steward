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
		"sub": "alice@example.com",
		"steward": {
			"acting_as": "user",
			"runtime_uid": "runtime-uid-a",
			"tools": [{
				"provider": "github",
				"resource": "search_repositories",
				"action": "read",
			}],
			"version": 1,
		},
	},
}

test_runtime_tool_grant_allows_the_exact_tool if {
	data.steward.mcp.allow with input as allowed_input
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

test_gateway_contract_explains_a_subject_mismatch if {
	claims := object.union(allowed_input.tokenClaims, {"sub": "bob@example.org"})
	request := object.union(allowed_input, {"tokenClaims": claims})
	decision := data.steward.mcp.decision with input as request
	object.get(decision, "reason", "") == "verified token subject does not match the request"
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
