const { expect, test } = require("@playwright/test");
const { navigateAndWait, watchPageErrors } = require("../helpers");

async function mockSkillsApi(page, fixture) {
	await page.route("**/api/skills/evals/*", (route) => {
		var url = new URL(route.request().url());
		var id = url.pathname.split("/").pop();
		var byId = fixture.evalRunDetailById || {};
		var detail = byId[id] || fixture.evalRunDetail || {};
		return route.fulfill({
			status: 200,
			contentType: "application/json",
			body: JSON.stringify(detail),
		});
	});
	await page.route("**/api/skills/evals", (route) => {
		return route.fulfill({
			status: 200,
			contentType: "application/json",
			body: JSON.stringify({ runs: fixture.evalRuns || [] }),
		});
	});
	await page.route("**/api/skills/search**", (route) => {
		return route.fulfill({
			status: 200,
			contentType: "application/json",
			body: JSON.stringify({ skills: fixture.searchSkills || [] }),
		});
	});
	await page.route("**/api/skills", (route) => {
		return route.fulfill({
			status: 200,
			contentType: "application/json",
			body: JSON.stringify({
				skills: fixture.enabledSkills || [],
				repos: fixture.repos || [],
			}),
		});
	});
}

async function installSkillsRpcMock(page, responses) {
	await page.addInitScript((mockResponses) => {
		window.__skillsRpcCalls = [];
		if (window.__skillsWsMockInstalled) return;

		var originalSend = WebSocket.prototype.send;
		WebSocket.prototype.send = function (data) {
			try {
				var frame = JSON.parse(data);
				if (frame?.type === "req" && frame.method) {
					window.__skillsRpcCalls.push({
						method: frame.method,
						params: frame.params || {},
					});

					var mocked = mockResponses[frame.method];
					if (mocked) {
						var responseFrame = Object.assign({ type: "res", id: frame.id, ok: true, payload: {} }, mocked, {
							id: frame.id,
							type: "res",
						});
						setTimeout(() => {
							this.dispatchEvent(
								new MessageEvent("message", {
									data: JSON.stringify(responseFrame),
								}),
							);
						}, 0);
						return;
					}
				}
			} catch {
				// ignore non-JSON websocket frames
			}

			return originalSend.call(this, data);
		};

		window.__skillsWsMockInstalled = true;
	}, responses);
}

async function expandRepo(page, source) {
	await page.locator(".skills-repo-header").filter({ hasText: source }).click();
}

async function openSkillDetail(page, displayName) {
	await page.locator(".skills-ac-item").filter({ hasText: displayName }).click();
}

test.describe("Skills page", () => {
	test("skills page loads", async ({ page }) => {
		const pageErrors = watchPageErrors(page);
		await navigateAndWait(page, "/skills");

		await expect(page.getByRole("heading", { name: "Skills", exact: true })).toBeVisible();
		expect(pageErrors).toEqual([]);
	});

	test("install input present", async ({ page }) => {
		await navigateAndWait(page, "/skills");

		await expect(page.getByPlaceholder("owner/repo or full URL (e.g. anthropics/skills)")).toBeVisible();
		await expect(page.getByRole("button", { name: "Install", exact: true }).first()).toBeVisible();
	});

	test("featured repos shown", async ({ page }) => {
		await navigateAndWait(page, "/skills");

		await expect(page.getByRole("heading", { name: "Featured Repositories", exact: true })).toBeVisible();
		await expect(page.getByRole("link", { name: "openclaw/skills", exact: true })).toBeVisible();
	});

	test("page has no JS errors", async ({ page }) => {
		const pageErrors = watchPageErrors(page);
		await navigateAndWait(page, "/skills");
		expect(pageErrors).toEqual([]);
	});

	test("quarantined badges render for repo and skill entries", async ({ page }) => {
		await mockSkillsApi(page, {
			repos: [
				{
					source: "owner/repo",
					repo_name: "owner-repo",
					skill_count: 1,
					enabled_count: 0,
					quarantined_count: 1,
				},
			],
			searchSkills: [
				{
					name: "bad-skill",
					display_name: "Bad Skill",
					description: "Quarantined test fixture",
					trusted: false,
					status: "quarantined",
					quarantined: true,
					enabled: false,
					eligible: true,
				},
			],
		});

		await navigateAndWait(page, "/skills");
		await expect(page.locator(".skills-repo-card").filter({ hasText: "1 quarantined" })).toBeVisible();

		await expandRepo(page, "owner/repo");
		await expect(page.locator(".skills-ac-item").filter({ hasText: "Bad Skill" })).toBeVisible();
		await expect(
			page.locator(".skills-ac-item").filter({ hasText: "Bad Skill" }).locator("text=quarantined"),
		).toBeVisible();
	});

	test("quarantined skill action requires unquarantine RPC", async ({ page }) => {
		await mockSkillsApi(page, {
			repos: [
				{
					source: "owner/repo",
					repo_name: "owner-repo",
					skill_count: 1,
					enabled_count: 0,
					quarantined_count: 1,
				},
			],
			searchSkills: [
				{
					name: "bad-skill",
					display_name: "Bad Skill",
					description: "Quarantined test fixture",
					trusted: false,
					status: "quarantined",
					quarantined: true,
					enabled: false,
					eligible: true,
				},
			],
		});
		await installSkillsRpcMock(page, {
			"skills.skill.detail": {
				ok: true,
				payload: {
					name: "bad-skill",
					display_name: "Bad Skill",
					source: "owner/repo",
					description: "Quarantined detail fixture",
					trusted: false,
					status: "quarantined",
					quarantined: true,
					quarantine_reason: "scanner high severity finding",
					enabled: false,
					eligible: true,
					missing_bins: [],
					body_html: "<p>fixture</p>",
				},
			},
			"skills.skill.unquarantine": {
				ok: true,
				payload: { ok: true },
			},
		});

		await navigateAndWait(page, "/skills");
		await expandRepo(page, "owner/repo");
		await openSkillDetail(page, "Bad Skill");

		await expect(page.getByText("Quarantined:", { exact: false }).first()).toBeVisible();
		await expect(page.getByRole("button", { name: "Unquarantine", exact: true }).first()).toBeVisible();

		await page.getByRole("button", { name: "Unquarantine", exact: true }).first().click();
		await expect(page.locator(".modal-overlay")).toBeVisible();
		await expect(page.locator(".modal-overlay")).toContainText('Unquarantine skill "bad-skill"?');
		await page.locator(".modal-overlay").getByRole("button", { name: "Unquarantine", exact: true }).click();

		await expect
			.poll(() =>
				page.evaluate(() => (window.__skillsRpcCalls || []).some((c) => c.method === "skills.skill.unquarantine")),
			)
			.toBe(true);
		await expect
			.poll(() => page.evaluate(() => (window.__skillsRpcCalls || []).some((c) => c.method === "skills.skill.enable")))
			.toBe(false);
	});

	test("untrusted non-quarantined skill requires separate trust then enable", async ({ page }) => {
		await mockSkillsApi(page, {
			repos: [
				{
					source: "owner/repo",
					repo_name: "owner-repo",
					skill_count: 1,
					enabled_count: 0,
					quarantined_count: 0,
				},
			],
			searchSkills: [
				{
					name: "safe-skill",
					display_name: "Safe Skill",
					description: "Untrusted fixture",
					trusted: false,
					status: "untrusted",
					quarantined: false,
					enabled: false,
					eligible: true,
				},
			],
		});
		await installSkillsRpcMock(page, {
			"skills.skill.detail": {
				ok: true,
				payload: {
					name: "safe-skill",
					display_name: "Safe Skill",
					source: "owner/repo",
					description: "Untrusted detail fixture",
					trusted: false,
					status: "untrusted",
					quarantined: false,
					enabled: false,
					eligible: true,
					missing_bins: [],
					body_html: "<p>fixture</p>",
				},
			},
			"skills.skill.trust": {
				ok: true,
				payload: { ok: true },
			},
			"skills.skill.enable": {
				ok: true,
				payload: { ok: true },
			},
		});

		await navigateAndWait(page, "/skills");
		await expandRepo(page, "owner/repo");
		await openSkillDetail(page, "Safe Skill");

		await page.getByRole("button", { name: "Trust", exact: true }).first().click();
		await expect(page.locator(".modal-overlay")).toContainText('Trust skill "safe-skill" from owner/repo?');
		await page.locator(".modal-overlay").getByRole("button", { name: "Trust", exact: true }).click();

		await expect
			.poll(() =>
				page.evaluate(() => {
					var methods = (window.__skillsRpcCalls || []).map((c) => c.method);
					var trustIndex = methods.indexOf("skills.skill.trust");
					var enableIndex = methods.indexOf("skills.skill.enable");
					return trustIndex >= 0 && enableIndex === -1;
				}),
			)
			.toBe(true);

		// Refresh detail after trust mock and enable explicitly as a second action.
		await page
			.evaluate(() => {
				window.__skillsRpcCalls = [];
			})
			.catch(() => null);
		await page.reload();
		await expandRepo(page, "owner/repo");
		await openSkillDetail(page, "Safe Skill");
		await page.getByRole("button", { name: "Enable", exact: true }).first().click();
		await expect
			.poll(() => page.evaluate(() => (window.__skillsRpcCalls || []).some((c) => c.method === "skills.skill.enable")))
			.toBe(true);
		await expect
			.poll(() =>
				page.evaluate(() => (window.__skillsRpcCalls || []).some((c) => c.method === "skills.skill.unquarantine")),
			)
			.toBe(false);
	});

	test("skill eval benchmark can be started from skills page", async ({ page }) => {
		await mockSkillsApi(page, {
			enabledSkills: [
				{
					name: "skill-creator",
					description: "Create and improve skills",
					source: "anthropics/skills",
					enabled: true,
				},
			],
			repos: [],
			evalRuns: [],
		});
		await installSkillsRpcMock(page, {
			"skills.evals.run": {
				ok: true,
				payload: {
					id: "eval-123",
					skill_name: "skill-creator",
					source: "anthropics/skills",
					status: "completed",
					benchmark: {
						run_summary: {
							with_skill_pass_rate: 0.86,
							without_skill_pass_rate: 0.43,
							pass_rate_delta: 0.43,
						},
						assertions: [],
						notes: ["With-skill pass rate is +43.0% higher than baseline."],
					},
				},
			},
		});

		await navigateAndWait(page, "/skills");
		await expect(page.getByRole("heading", { name: "Skill Evals", exact: true })).toBeVisible();
		await page
			.getByRole("button", { name: /skill-creator/i })
			.first()
			.click();
		await page.getByRole("button", { name: "Run Benchmark", exact: true }).click();

		await expect
			.poll(() => page.evaluate(() => (window.__skillsRpcCalls || []).some((c) => c.method === "skills.evals.run")))
			.toBe(true);
	});

	test("skill eval row loads benchmark detail panel", async ({ page }) => {
		await mockSkillsApi(page, {
			enabledSkills: [],
			repos: [],
			evalRuns: [
				{
					id: "eval-xyz",
					skill_name: "skill-creator",
					source: "anthropics/skills",
					with_skill_pass_rate: 0.8,
					without_skill_pass_rate: 0.4,
					pass_rate_delta: 0.4,
					created_at_ms: 1762300000000,
				},
			],
			evalRunDetailById: {
				"eval-xyz": {
					id: "eval-xyz",
					benchmark: {
						run_summary: {
							pass_rate_delta: 0.4,
						},
						notes: ["With-skill pass rate is +40.0% higher than baseline."],
						assertions: [
							{
								id: "workflow_steps",
								label: "Workflow is stepwise and executable",
								with_skill: { passed: true },
								without_skill: { passed: false },
							},
						],
					},
				},
			},
		});

		await navigateAndWait(page, "/skills");
		await page.locator("tbody tr").filter({ hasText: "skill-creator" }).first().click();
		await expect(page.getByText("Eval eval-xyz", { exact: false })).toBeVisible();
		await expect(page.getByText("Workflow is stepwise and executable", { exact: true })).toBeVisible();
	});
});
