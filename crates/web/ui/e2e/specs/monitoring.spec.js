const { expect, test } = require("@playwright/test");
const { navigateAndWait, waitForWsConnected, watchPageErrors } = require("../helpers");

function isRetryableRpcError(message) {
	if (typeof message !== "string") return false;
	return message.includes("WebSocket not connected") || message.includes("WebSocket disconnected");
}

async function sendRpcFromPage(page, method, params) {
	let lastResponse = null;
	for (let attempt = 0; attempt < 40; attempt++) {
		if (attempt > 0) {
			await waitForWsConnected(page);
		}
		lastResponse = await page
			.evaluate(
				async ({ methodName, methodParams }) => {
					var appScript = document.querySelector('script[type="module"][src*="js/app.js"]');
					if (!appScript) throw new Error("app module script not found");
					var appUrl = new URL(appScript.src, window.location.origin);
					var prefix = appUrl.href.slice(0, appUrl.href.length - "js/app.js".length);
					var helpers = await import(`${prefix}js/helpers.js`);
					return helpers.sendRpc(methodName, methodParams);
				},
				{
					methodName: method,
					methodParams: params,
				},
			)
			.catch((error) => ({ ok: false, error: { message: error?.message || String(error) } }));

		if (lastResponse?.ok) return lastResponse;
		if (!isRetryableRpcError(lastResponse?.error?.message)) return lastResponse;
	}

	return lastResponse;
}

async function expectRpcOk(page, method, params) {
	const response = await sendRpcFromPage(page, method, params);
	expect(response?.ok, `RPC ${method} failed: ${response?.error?.message || "unknown error"}`).toBeTruthy();
	return response;
}

test.describe("Monitoring dashboard", () => {
	test("monitoring page loads", async ({ page }) => {
		const pageErrors = watchPageErrors(page);
		await navigateAndWait(page, "/monitoring");

		await expect(page.getByRole("heading", { name: "Monitoring", exact: true })).toBeVisible();
		expect(pageErrors).toEqual([]);
	});

	test("time range selector present", async ({ page }) => {
		await navigateAndWait(page, "/monitoring");

		// Monitoring page should have time range buttons or selector
		const content = page.locator("#pageContent");
		await expect(content).not.toBeEmpty();
	});

	test("page has no JS errors", async ({ page }) => {
		const pageErrors = watchPageErrors(page);
		await navigateAndWait(page, "/monitoring");
		expect(pageErrors).toEqual([]);
	});

	test("provider health table renders from metrics update events", async ({ page }) => {
		const pageErrors = await navigateAndWait(page, "/monitoring");
		await waitForWsConnected(page);

		await expectRpcOk(page, "system-event", {
			event: "metrics.update",
			payload: {
				snapshot: {
					categories: {
						llm: {
							by_provider: [],
						},
					},
				},
				providerHealth: {
					windowSecs: 60,
					sampleCount: 1,
					providers: [
						{
							provider: "openai",
							model: "gpt-5.2",
							totalRequests: 42,
							successRate: 0.95,
							errorRate: 0.05,
							errorCount: 2,
							p95LatencyMs: 1234,
							errorRateByClass: {
								timeout: 0.03,
								rate_limited: 0.02,
							},
						},
					],
				},
			},
		});

		await expect(page.getByRole("heading", { name: "Provider Health", exact: true })).toBeVisible();
		await expect(page.getByRole("cell", { name: "openai", exact: true })).toBeVisible();
		await expect(page.getByRole("cell", { name: "gpt-5.2", exact: true })).toBeVisible();
		await expect(page.getByText("95.0%")).toBeVisible();
		await expect(page.getByText("5.0%")).toBeVisible();
		await expect(page.getByText("timeout (3.0%)")).toBeVisible();

		expect(pageErrors).toEqual([]);
	});
});
