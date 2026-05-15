#!/usr/bin/env node

import { readFile } from "node:fs/promises";
import { Agent, CursorAgentError } from "@cursor/sdk";

const eventPath = requireEnv("GITHUB_EVENT_PATH");
const githubToken = requireEnv("GITHUB_TOKEN");

const event = JSON.parse(await readFile(eventPath, "utf8"));
const issue = event.issue;
const repository = event.repository;

if (!issue || !repository) {
  throw new Error("This automation must be run from a GitHub issue event.");
}

const owner = repository.owner.login;
const repoName = repository.name;
const issueNumber = issue.number;
const repoUrl = repository.html_url;
const startingRef = repository.default_branch ?? process.env.GITHUB_REF_NAME ?? "main";

await main();

async function main() {
  try {
    console.log(`Triaging issue #${issueNumber}: ${issue.title}`);

    const triageResult = await runCursorAgent({
      autoCreatePR: false,
      prompt: buildTriagePrompt(),
    });

    const triage = parseTriageResult(triageResult.result ?? "");
    console.log(`Triage classification: ${triage.classification}`);

    if (triage.classification !== "real") {
      await postIssueComment(formatNoPrComment(triage, triageResult));
      return;
    }

    console.log(`Issue #${issueNumber} looks real; starting fix agent.`);
    const fixResult = await runCursorAgent({
      autoCreatePR: true,
      prompt: buildFixPrompt(triage),
    });

    const prUrls = extractPrUrls(fixResult);
    await postIssueComment(formatFixComment(triage, fixResult, prUrls));
  } catch (error) {
    await postIssueComment(formatFailureComment(error));
    throw error;
  }
}

async function runCursorAgent({ autoCreatePR, prompt }) {
  let agent;

  try {
    agent = await Agent.create({
      apiKey: requireEnv("CURSOR_API_KEY"),
      cloud: {
        repos: [{ url: repoUrl, startingRef }],
        autoCreatePR,
        skipReviewerRequest: true,
      },
    });

    const run = await agent.send(prompt);
    console.log(`Cursor agent: ${agent.agentId}`);
    console.log(`Cursor run: ${run.id}`);

    const result = await run.wait();
    console.log(`Cursor run status: ${result.status}`);

    if (result.status !== "finished") {
      throw new Error(`Cursor run did not finish successfully: ${result.status}`);
    }

    return result;
  } catch (error) {
    if (error instanceof CursorAgentError) {
      throw new Error(
        `Cursor agent failed to start: ${error.message} (retryable: ${error.isRetryable})`,
      );
    }

    throw error;
  } finally {
    if (agent) {
      await agent[Symbol.asyncDispose]();
    }
  }
}

function buildTriagePrompt() {
  return `
You are an unattended issue triage agent for this Rust/Solana repository.

Repository: ${repoUrl}
Default branch: ${startingRef}
Issue: #${issueNumber}
Title: ${issue.title}
Author: ${issue.user?.login ?? "unknown"}
Labels: ${issue.labels?.map((label) => label.name).join(", ") || "none"}
URL: ${issue.html_url}

Issue body:
${issue.body || "(no body provided)"}

Your task:
1. Read the repository and decide whether this issue is a real, actionable bug or implementation defect.
2. Attempt a minimal reproduction when feasible. Prefer static code evidence when runtime reproduction would require unavailable external services.
3. Do not modify files, do not commit, and do not open a pull request during triage.
4. Classify vague feature requests, duplicates, missing reproduction reports, environment-only problems, and unsupported-chain requests as "needs_info" or "not_real".
5. Only classify as "real" when there is enough evidence to justify a code change.

Return only compact JSON with this exact shape:
{
  "classification": "real" | "not_real" | "needs_info",
  "confidence": 0.0,
  "summary": "short human-readable conclusion",
  "evidence": ["specific file/function/behavior evidence"],
  "recommended_fix": "short fix direction, or empty string"
}
`;
}

function buildFixPrompt(triage) {
  return `
You are an unattended fix agent for this Rust/Solana repository.

Repository: ${repoUrl}
Default branch: ${startingRef}
Issue: #${issueNumber}
Title: ${issue.title}
URL: ${issue.html_url}

Issue body:
${issue.body || "(no body provided)"}

Triage result:
${JSON.stringify(triage, null, 2)}

The issue has been classified as real. Implement the smallest correct fix.

Requirements:
1. Keep changes focused on the issue. Do not perform broad refactors.
2. Do not edit vendored dependencies under vendor/ unless the issue directly requires it.
3. Add or update focused tests when practical.
4. Run validation where feasible:
   - cargo fmt --all --check
   - cargo clippy --workspace --all-targets
   - cargo test --workspace
5. If validation cannot run because of environment constraints, explain that in the final response.
6. Create a pull request with a clear summary and include "Fixes #${issueNumber}" in the PR body.
7. If you discover the triage was wrong and no code change is justified, do not create a PR; explain why.
`;
}

function parseTriageResult(text) {
  const jsonText = extractJsonObject(text);

  if (!jsonText) {
    throw new Error(`Cursor triage did not return JSON. Result: ${text}`);
  }

  const parsed = JSON.parse(jsonText);
  const validClassifications = new Set(["real", "not_real", "needs_info"]);

  if (!validClassifications.has(parsed.classification)) {
    throw new Error(`Invalid triage classification: ${parsed.classification}`);
  }

  return {
    classification: parsed.classification,
    confidence: Number(parsed.confidence ?? 0),
    summary: String(parsed.summary ?? ""),
    evidence: Array.isArray(parsed.evidence) ? parsed.evidence.map(String) : [],
    recommended_fix: String(parsed.recommended_fix ?? ""),
  };
}

function extractJsonObject(text) {
  const trimmed = text.trim();

  if (trimmed.startsWith("{") && trimmed.endsWith("}")) {
    return trimmed;
  }

  const fenced = trimmed.match(/```(?:json)?\s*([\s\S]*?)\s*```/i);
  if (fenced?.[1]?.trim().startsWith("{")) {
    return fenced[1].trim();
  }

  const start = trimmed.indexOf("{");
  const end = trimmed.lastIndexOf("}");
  return start >= 0 && end > start ? trimmed.slice(start, end + 1) : null;
}

function extractPrUrls(result) {
  const branchUrls =
    result.git?.branches?.map((branch) => branch.prUrl).filter(Boolean) ?? [];
  const textUrls = (result.result ?? "").match(
    /https:\/\/github\.com\/[^\s)]+\/pull\/\d+/g,
  );

  return [...new Set([...branchUrls, ...(textUrls ?? [])])];
}

async function postIssueComment(body) {
  const response = await fetch(
    `https://api.github.com/repos/${owner}/${repoName}/issues/${issueNumber}/comments`,
    {
      method: "POST",
      headers: {
        Accept: "application/vnd.github+json",
        Authorization: `Bearer ${githubToken}`,
        "Content-Type": "application/json",
        "X-GitHub-Api-Version": "2022-11-28",
      },
      body: JSON.stringify({ body }),
    },
  );

  if (!response.ok) {
    const responseBody = await response.text();
    throw new Error(
      `Failed to post issue comment: ${response.status} ${response.statusText}\n${responseBody}`,
    );
  }
}

function formatNoPrComment(triage, result) {
  const evidence = triage.evidence.length
    ? triage.evidence.map((item) => `- ${item}`).join("\n")
    : "- No concrete evidence found.";

  return `AI triage completed. No pull request was opened.

Classification: \`${triage.classification}\`
Confidence: ${formatConfidence(triage.confidence)}

${triage.summary}

Evidence:
${evidence}

Cursor result:
${result.result ?? "(no final message)"}`;
}

function formatFixComment(triage, result, prUrls) {
  const prText = prUrls.length
    ? prUrls.map((url) => `- ${url}`).join("\n")
    : "- No pull request URL was returned by the Cursor run.";

  return `AI verified this issue as real and attempted a fix.

Triage summary: ${triage.summary}

Pull request:
${prText}

Cursor result:
${result.result ?? "(no final message)"}`;
}

function formatFailureComment(error) {
  return `AI issue automation failed before it could complete.

Error:
\`\`\`
${error?.stack ?? error?.message ?? String(error)}
\`\`\``;
}

function formatConfidence(confidence) {
  if (!Number.isFinite(confidence)) {
    return "unknown";
  }

  return `${Math.round(confidence * 100)}%`;
}

function requireEnv(name) {
  const value = process.env[name];
  if (!value) {
    throw new Error(`Missing required environment variable: ${name}`);
  }

  return value;
}
