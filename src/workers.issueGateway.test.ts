import { describe, expect, it } from "vitest";
import gatewayWorkers from "../workers/lookback-issue-gateway-workers.yaml?raw";

describe("issue gateway worker contract", () => {
  it("exposes only the five reflected claim-lifecycle RPCs", () => {
    expect(gatewayWorkers).toContain("issue_tracker.rpc.v1.IssueGateway/");
    expect(gatewayWorkers).not.toContain("LookbackIssueGateway");
    expect(gatewayWorkers).not.toContain("lookback_issue_gateway");
    for (const method of ["ListReady", "Claim", "Heartbeat", "Release", "Transition"]) {
      expect(gatewayWorkers).toContain(`IssueGateway/${method}`);
    }
    for (const forbidden of ["Get", "List", "Create", "Update", "AddComment", "AddRelation"]) {
      expect(gatewayWorkers).not.toMatch(new RegExp(`IssueGateway/${forbidden}(?:\\n|\\r|$|\\")`));
    }
    expect(gatewayWorkers.match(/use_reflection: true/g) ?? []).toHaveLength(5);
  });
});
