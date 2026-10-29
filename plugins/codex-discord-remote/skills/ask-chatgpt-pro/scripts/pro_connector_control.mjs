export const PROTOCOL = "ask-chatgpt-pro-connector-control-v1";
export const CONNECTOR_NAME = "Simdorei Local Project Oauth";
export const CONNECTOR_PATH =
  "/plugins/plugin_asdk_app_6a6ae90be0a08191b877eddba93b631c";
const CONNECTOR_IMPRESSION_ID = CONNECTOR_PATH.slice("/plugins/".length);
const PLUGIN_BUTTON_NAMES = ["플러그인", "Plugins"];
const PLUGIN_BUTTON_TIMEOUT_MS = 10000;
const READINESS_POLL_MS = 100;
const CONNECTOR_PILL_SELECTOR = [
  `a[href="${CONNECTOR_PATH}"]`,
  `a[href^="${CONNECTOR_PATH}?"]`,
  `a[href^="${CONNECTOR_PATH}#"]`,
].join(", ");

function failed(stage) {
  return {
    protocol: PROTOCOL,
    browser_type: "chrome",
    status: "failed",
    connector_name: CONNECTOR_NAME,
    connector_path: CONNECTOR_PATH,
    chat_mode: "unverified",
    pro_mode: false,
    action: "none",
    failed_stage: stage,
  };
}

async function observeComposer(composer) {
  const pill = composer.locator(CONNECTOR_PILL_SELECTOR);
  const pillCountBefore = await pill.count();
  if (pillCountBefore > 1) return { status: "duplicate" };
  const textBefore = (await composer.textContent()) ?? "";
  const pillTextBefore =
    pillCountBefore === 1 ? ((await pill.textContent()) ?? "") : "";

  const textAfter = (await composer.textContent()) ?? "";
  const pillCountAfter = await pill.count();
  if (pillCountAfter > 1) return { status: "duplicate" };
  const pillTextAfter =
    pillCountAfter === 1 ? ((await pill.textContent()) ?? "") : "";

  if (
    textBefore !== textAfter ||
    pillCountBefore !== pillCountAfter ||
    pillTextBefore !== pillTextAfter
  ) {
    return { status: "changed" };
  }
  return {
    status: "stable",
    text: textAfter,
    pillCount: pillCountAfter,
    pillText: pillTextAfter,
  };
}

async function observeStableComposer(composer) {
  const first = await observeComposer(composer);
  if (first.status !== "stable") return first;
  const second = await observeComposer(composer);
  if (second.status !== "stable") return second;
  if (
    first.text !== second.text ||
    first.pillCount !== second.pillCount ||
    first.pillText !== second.pillText
  ) {
    return { status: "changed" };
  }
  return second;
}

async function observeReadyComposer(composer, composerSurface = null) {
  const state = await observeStableComposer(composer);
  if (state.status !== "stable") return state;

  let textWithoutPill = state.text;
  if (state.pillText) {
    textWithoutPill = textWithoutPill.replace(state.pillText, "");
  }
  if (textWithoutPill.trim()) return { status: "not_empty" };

  if (composerSurface === null) {
    return { status: "ready", pillCount: state.pillCount };
  }
  const surfacePill = composerSurface.locator(CONNECTOR_PILL_SELECTOR);
  const surfacePillCount = await surfacePill.count();
  if (surfacePillCount > 1) return { status: "duplicate" };
  if (surfacePillCount !== state.pillCount) return { status: "changed" };
  return { status: "ready", pillCount: surfacePillCount };
}

function composerFailureStage(state) {
  if (state.status === "duplicate") return "connector_pill";
  if (state.status === "not_empty") return "composer_not_empty";
  if (state.status !== "ready") return "composer_changed";
  return null;
}

function remainingTimeoutMs(deadlineMs) {
  return Math.max(0, deadlineMs - Date.now());
}

async function waitForVisible(locator, deadlineMs) {
  const timeoutMs = remainingTimeoutMs(deadlineMs);
  if (timeoutMs === 0) return false;
  try {
    await locator.waitFor({ state: "visible", timeoutMs });
    return true;
  } catch {
    return false;
  }
}

async function waitForWorkSelected(playwright, deadlineMs) {
  while (remainingTimeoutMs(deadlineMs) > 0) {
    const work = playwright.getByRole("radio", {
      name: "Work",
      exact: true,
    });
    const count = await work.count();
    if (count > 1) return null;
    if (count === 1 && (await work.getAttribute("aria-checked")) === "true") {
      return work;
    }
    await playwright.waitForTimeout(
      Math.min(READINESS_POLL_MS, remainingTimeoutMs(deadlineMs)),
    );
  }
  return null;
}

function pluginButtonCandidates(playwright) {
  return PLUGIN_BUTTON_NAMES.map((name) => ({
    name,
    locator: playwright.getByRole("button", {
      name,
      exact: true,
    }),
  }));
}

async function waitForAnyPluginButton(playwright, deadlineMs) {
  try {
    await Promise.any(
      pluginButtonCandidates(playwright).map(async ({ locator }) => {
        if (!(await waitForVisible(locator, deadlineMs))) {
          throw new Error("plugin button not visible");
        }
      }),
    );
    return true;
  } catch {
    return false;
  }
}

async function observePluginButton(playwright) {
  let matched = null;
  for (const candidate of pluginButtonCandidates(playwright)) {
    const count = await candidate.locator.count();
    if (count > 1) return { status: "duplicate" };
    if (count === 0 || !(await candidate.locator.isVisible())) continue;
    if (matched !== null) return { status: "duplicate" };
    matched = {
      ...candidate,
      menu: (await candidate.locator.getAttribute("aria-haspopup")) === "menu",
    };
  }
  if (matched === null) return { status: "missing" };
  return { status: matched.menu ? "ready" : "invalid", ...matched };
}

async function findUniquePluginButton(playwright, deadlineMs) {
  if (!(await waitForAnyPluginButton(playwright, deadlineMs))) return null;

  let previousName = null;
  while (remainingTimeoutMs(deadlineMs) > 0) {
    const current = await observePluginButton(playwright);
    if (current.status === "duplicate") return null;
    if (current.status === "ready") {
      if (current.name === previousName) return current.locator;
      previousName = current.name;
    } else {
      previousName = null;
    }
    await playwright.waitForTimeout(
      Math.min(READINESS_POLL_MS, remainingTimeoutMs(deadlineMs)),
    );
  }
  return null;
}

// Current ChatGPT composer: a picker label can request an attachment, but only
// the exact resulting chip identity is evidence. Never submit from this helper.
const CURRENT_COMPOSER =
  '[contenteditable="true"][role="textbox"][data-composer-markdown]';
const CURRENT_SURFACE = 'form[data-thread-find-composer="true"]';
const CURRENT_ADD_CONTEXT = 'button[data-composer-navigation-target="add-context"]';
const CURRENT_MODEL =
  'button[data-composer-navigation-target="reasoning"]:is([data-codex-intelligence-trigger="true"], [aria-label="ChatGPT 모델 선택"])';
const CURRENT_CATALOG_HEADING = CONNECTOR_NAME + " 내 클라우드 플러그인";
const CURRENT_CONNECTOR_CHIP = [
  'span[contenteditable="false"]',
  '[plugin-mention-name="dev-6a6ae90be0a08191b877eddba93b631c"]',
  '[plugin-mention-display-name="Simdorei Local Project Oauth"]',
  '[plugin-mention-path="plugin://dev-6a6ae90be0a08191b877eddba93b631c@openai-curated-remote"]',
  '[data-prompt-link-href="plugin://dev-6a6ae90be0a08191b877eddba93b631c@openai-curated-remote"]',
].join("");
const CURRENT_CONNECTOR_APP_CHIP = [
  'span[contenteditable="false"]',
  '[app-mention-name="simdorei-local-project-oauth"]',
  '[app-mention-display-name="Simdorei Local Project Oauth"]',
  '[app-mention-path="app://asdk_app_6a6ae90be0a08191b877eddba93b631c"]',
  '[data-prompt-link-href="app://asdk_app_6a6ae90be0a08191b877eddba93b631c"]',
].join("");
const CURRENT_CONNECTOR_PILL = [
  CONNECTOR_PILL_SELECTOR, CURRENT_CONNECTOR_CHIP, CURRENT_CONNECTOR_APP_CHIP,
].join(", ");

async function isPlainCurrentChat(tab, composer, surface) {
  return (await surface.getAttribute("data-chatgpt-composer")) === "" &&
    (await surface.getAttribute("data-composer-placement")) === "thread" &&
    (await composer.getAttribute("aria-label")) === "ChatGPT에게 물어보세요" &&
    (await surface.locator(CURRENT_ADD_CONTEXT).count()) === 1 &&
    (await surface.getByRole("button", { name: "Work", exact: true }).count()) === 0 &&
    (await surface.getByRole("button", { name: "Chat", exact: true }).count()) === 0 &&
    (await tab.playwright.getByRole("radio", { name: "Work", exact: true }).count()) === 0 &&
    (await tab.playwright.getByRole("radio", { name: "Chat", exact: true }).count()) === 0;
}

async function observeCurrentComposer(composer, surface) {
  const read = async () => {
    if ((await composer.count()) !== 1 || (await surface.count()) !== 1 ||
        (await surface.locator(CURRENT_COMPOSER).count()) !== 1) {
      return { failure: "composer_changed" };
    }
    const pill = composer.locator(CURRENT_CONNECTOR_PILL);
    const pillCount = await pill.count();
    const surfaceCount = await surface.locator(CURRENT_CONNECTOR_PILL).count();
    const chipCount = await composer.locator(CURRENT_CONNECTOR_CHIP).count();
    const allChips = await composer.locator("span[plugin-mention-path]").count();
    const appChipCount = await composer.locator(CURRENT_CONNECTOR_APP_CHIP).count();
    const allAppChips = await composer.locator("span[app-mention-path]").count();
    const anchorCount = await composer.locator(CONNECTOR_PILL_SELECTOR).count();
    const allAnchors = await composer.locator('a[href^="/plugins/"]').count();
    if (pillCount > 1 || surfaceCount > 1 || allChips !== chipCount ||
        allAppChips !== appChipCount ||
        allAnchors !== anchorCount) return { failure: "connector_pill" };
    if (pillCount !== surfaceCount) return { failure: "composer_changed" };
    const text = (await composer.textContent()) ?? "";
    const pillText = pillCount === 1 ? (await pill.textContent()) ?? "" : "";
    if (pillCount === 1 &&
        (pillText.trim() !== CONNECTOR_NAME || !text.includes(pillText))) {
      return { failure: "connector_pill" };
    }
    return { pillCount, text, pillText };
  };
  const first = await read();
  if (first.failure) return first;
  const second = await read();
  if (second.failure) return second;
  if (first.pillCount !== second.pillCount || first.text !== second.text ||
      first.pillText !== second.pillText) return { failure: "composer_changed" };
  const draft = second.pillCount === 1
    ? second.text.replace(second.pillText, "") : second.text;
  if (draft.trim()) return { failure: "composer_not_empty" };
  return { pillCount: second.pillCount, failure: null };
}

async function currentPickerAxTarget(tab) {
  if (typeof tab.ax?.get !== "function" || typeof tab.ax?.click !== "function") {
    return null;
  }
  const state = await tab.ax.get("state", { disableDiffing: true });
  const expected = `${CONNECTOR_NAME} ${CONNECTOR_NAME}`;
  const matches = state.split(/\r?\n/)
    .map(line => line.match(/^[\t ]*(\d+) button (.+)$/))
    .filter(match => match?.[2] === expected);
  if (matches.length !== 1) return null;
  const target = Number(matches[0][1]);
  return Number.isSafeInteger(target) ? target : null;
}

async function prepareCurrentConnector(tab) {
  let stage = "conversation_url";
  try {
    const initialUrl = new URL(await tab.url());
    if (initialUrl.protocol !== "https:" || initialUrl.hostname !== "chatgpt.com") {
      return failed(stage);
    }
    let launched = false;
    if (initialUrl.pathname.startsWith("/plugins/")) {
      stage = "catalog_identity";
      const heading = tab.playwright.getByRole("heading", {
        name: CURRENT_CATALOG_HEADING, exact: true,
      });
      const use = tab.playwright.getByRole("button", {
        name: "채팅에서 사용해 보기", exact: true,
      });
      if (initialUrl.pathname !== CONNECTOR_PATH || initialUrl.hash ||
          (await heading.count()) !== 1 || (await use.count()) !== 1 ||
          (await tab.playwright.locator(CURRENT_COMPOSER).count()) !== 0 ||
          (await tab.playwright.locator('[id="prompt-textarea"]').count()) !== 0) {
        return failed(stage);
      }
      stage = "catalog_navigation";
      await use.click();
      await tab.playwright.locator(CURRENT_COMPOSER).waitFor({
        state: "visible", timeoutMs: 10000,
      });
      launched = true;
    }

    const conversationUrl = new URL(await tab.url());
    if (conversationUrl.protocol !== "https:" ||
        conversationUrl.hostname !== "chatgpt.com" ||
        !(conversationUrl.pathname === "/" ||
          /^\/c\/[0-9a-f-]+$/i.test(conversationUrl.pathname))) {
      return failed("catalog_navigation");
    }
    stage = "composer";
    let composer = tab.playwright.locator(CURRENT_COMPOSER);
    let surface = tab.playwright.locator(CURRENT_SURFACE);
    if ((await composer.count()) !== 1 ||
        (await tab.playwright.locator('[id="prompt-textarea"]').count()) !== 0) {
      return failed(stage);
    }
    stage = "composer_surface";
    if ((await surface.count()) !== 1 ||
        (await surface.locator(CURRENT_COMPOSER).count()) !== 1) return failed(stage);

    if (launched) {
      stage = "catalog_attach";
      await surface.locator(CURRENT_CONNECTOR_PILL).waitFor({
        state: "visible", timeoutMs: 10000,
      });
    }
    let ready = await observeCurrentComposer(composer, surface);
    let readinessFailure = ready.failure;
    if (readinessFailure) return failed(readinessFailure);
    let pickerResult = null;
    if (ready.pillCount !== 1) {
      if (launched) return failed("catalog_attach");
      stage = "pro_mode";
      const model = surface.locator(CURRENT_MODEL);
      if ((await model.count()) !== 1 ||
          (await model.getAttribute("aria-haspopup")) !== "menu" ||
          !/^(?:추론 수준\s+)?Pro$/.test((await model.innerText()).trim()) ||
          (await model.getByText("Pro", { exact: true }).count()) !== 1) return failed(stage);

      stage = "connector_picker_unavailable";
      const plus = surface.locator(CURRENT_ADD_CONTEXT);
      if ((await plus.count()) !== 1) return failed(stage);
      const expanded = await plus.getAttribute("aria-expanded");
      if (!["true", "false"].includes(expanded)) return failed(stage);
      ready = await observeCurrentComposer(composer, surface);
      if (ready.failure) return failed(ready.failure);
      if (ready.pillCount !== 0) return failed("composer_changed");
      if ((await tab.url()) !== conversationUrl.href) return failed("conversation_url");
      if (expanded === "false") await plus.click();

      stage = "connector_match";
      const item = tab.playwright.getByRole("button", {
        name: `${CONNECTOR_NAME} ${CONNECTOR_NAME}`, exact: true,
      });
      await item.waitFor({ state: "visible", timeoutMs: 10000 });
      if ((await item.count()) !== 1 ||
          (await item.getAttribute("type")) !== "button" ||
          (await item.getAttribute("data-list-navigation-item")) !== "true") return failed(stage);
      const target = await currentPickerAxTarget(tab);
      if (target === null) return failed(stage);
      composer = tab.playwright.locator(CURRENT_COMPOSER);
      surface = tab.playwright.locator(CURRENT_SURFACE);
      ready = await observeCurrentComposer(composer, surface);
      if (ready.failure) return failed(ready.failure);
      if (ready.pillCount !== 0 ||
          (await surface.locator(CURRENT_ADD_CONTEXT).getAttribute("aria-expanded")) !== "true") {
        return failed("composer_changed");
      }
      if ((await tab.url()) !== conversationUrl.href) return failed("conversation_url");

      stage = "connector_attach";
      pickerResult = "verified_composer_picker";
      try {
        // Use this fresh exact accessibility target, never locator coordinates or keys.
        await tab.ax.click(target);
      } catch {
        // An activation can have succeeded. Observe once; never activate again.
        pickerResult = "verified_after_error";
      }
      composer = tab.playwright.locator(CURRENT_COMPOSER);
      surface = tab.playwright.locator(CURRENT_SURFACE);
      await surface.locator(CURRENT_CONNECTOR_PILL).waitFor({
        state: "visible", timeoutMs: 10000,
      });
      ready = await observeCurrentComposer(composer, surface);
      if (ready.failure) return failed(ready.failure);
      if (ready.pillCount !== 1) return failed(stage);
      if ((await tab.url()) !== conversationUrl.href) return failed("conversation_url");
    }

    stage = "chat_mode";
    const modes = tab.playwright.getByRole("group", {
      name: "작성기 모드", exact: true,
    });
    const chat = modes.getByRole("button", { name: "Chat", exact: true });
    const work = modes.getByRole("button", { name: "Work", exact: true });
    const modeCount = await modes.count();
    if (modeCount === 0) {
      if (!(await isPlainCurrentChat(tab, composer, surface))) return failed(stage);
    } else {
      if (modeCount !== 1 || (await chat.count()) !== 1 ||
          (await work.count()) !== 1) return failed(stage);
      const chatState = await chat.getAttribute("aria-pressed");
      const workState = await work.getAttribute("aria-pressed");
      if (!["true", "false"].includes(chatState) ||
          !["true", "false"].includes(workState) || chatState === workState) {
        return failed(stage);
      }
      if (chatState !== "true") await chat.click();
      if ((await chat.getAttribute("aria-pressed")) !== "true" ||
          (await work.getAttribute("aria-pressed")) !== "false") return failed(stage);
    }

    composer = tab.playwright.locator(CURRENT_COMPOSER);
    surface = tab.playwright.locator(CURRENT_SURFACE);
    if ((await composer.count()) !== 1 || (await surface.count()) !== 1 ||
        (await surface.locator(CURRENT_COMPOSER).count()) !== 1) {
      return failed("composer_surface");
    }
    stage = "connector_after_chat";
    if ((await surface.locator(CURRENT_CONNECTOR_PILL).count()) !== 1) {
      return failed(stage);
    }
    stage = "pro_mode";
    const model = surface.locator(CURRENT_MODEL);
    if ((await model.count()) !== 1 ||
        (await model.getAttribute("aria-haspopup")) !== "menu") return failed(stage);
    await model.getByText("Pro", { exact: true }).waitFor({
      state: "visible", timeoutMs: 10000,
    });
    if ((await model.getByText("Pro", { exact: true }).count()) !== 1 ||
        !/^(?:추론 수준\s+)?Pro$/.test((await model.innerText()).trim())) return failed(stage);

    ready = await observeCurrentComposer(composer, surface);
    readinessFailure = ready.failure;
    if (readinessFailure) return failed(readinessFailure);
    if (ready.pillCount !== 1) return failed("connector_after_chat");
    if ((await modes.count()) !== modeCount) return failed("chat_mode");
    if (modeCount === 0) {
      if (!(await isPlainCurrentChat(tab, composer, surface))) return failed("chat_mode");
    } else if ((await chat.getAttribute("aria-pressed")) !== "true" ||
        (await work.getAttribute("aria-pressed")) !== "false") return failed("chat_mode");
    if ((await tab.url()) !== conversationUrl.href) return failed("conversation_url");
    return {
      protocol: PROTOCOL,
      browser_type: "chrome",
      status: "verified",
      connector_name: CONNECTOR_NAME,
      connector_path: CONNECTOR_PATH,
      chat_mode: "chat",
      pro_mode: true,
      action: launched || pickerResult ? "attached" : "already_attached",
      click_result: launched ? "verified_catalog_launch" : pickerResult ?? "not_needed",
    };
  } catch {
    return failed(stage);
  }
}

export async function prepareProConnector(globals = globalThis) {
  const currentTab = globals.proConversationTab;
  if (currentTab?.playwright) {
    try {
      const currentEditorCount = await currentTab.playwright.locator(CURRENT_COMPOSER).count();
      const currentHeadingCount = await currentTab.playwright.getByRole("heading", {
        name: CURRENT_CATALOG_HEADING, exact: true,
      }).count();
      if (currentEditorCount > 0 || currentHeadingCount > 0) {
        return await prepareCurrentConnector(currentTab);
      }
    } catch {
      return failed("composer");
    }
  }
  const tab = globals.proConversationTab;
  if (!tab?.playwright) return failed("conversation_tab");

  let stage = "conversation_url";
  try {
    const initialUrl = new URL(await tab.url());
    if (initialUrl.protocol !== "https:" || initialUrl.hostname !== "chatgpt.com") {
      return failed(stage);
    }
    let launchedFromCatalog = false;
    // The caller owns a fresh conversation-map creation lease before binding a
    // catalog tab. Never navigate an existing chat here or create a fallback.
    if (initialUrl.pathname.startsWith("/plugins/")) {
      stage = "catalog_identity";
      if (initialUrl.pathname !== CONNECTOR_PATH || initialUrl.hash ||
          (await tab.playwright.locator('[id="prompt-textarea"]').count()) !== 0) {
        return failed(stage);
      }
      const heading = tab.playwright.getByRole("heading", {
        name: CONNECTOR_NAME, exact: true,
      });
      const useButton = tab.playwright.getByRole("button", {
        name: "채팅에서 사용해 보기", exact: true,
      });
      if ((await heading.count()) !== 1 || (await useButton.count()) !== 1) {
        return failed(stage);
      }
      stage = "catalog_launch";
      await useButton.click();
      await tab.playwright.locator('[id="prompt-textarea"]').waitFor({
        state: "visible", timeoutMs: 10000,
      });
      stage = "catalog_navigation";
      const chatUrl = new URL(await tab.url());
      if (chatUrl.protocol !== "https:" || chatUrl.hostname !== "chatgpt.com" ||
          (chatUrl.pathname !== "/" && !/^\/c\/[0-9a-f-]+$/.test(chatUrl.pathname))) {
        return failed(stage);
      }
      stage = "catalog_attach";
      // The composer can render the label before its inline link hydrates.
      // Wait for the exact attachment, then apply the unchanged draft guard.
      await tab.playwright.locator('[data-composer-surface="true"]')
        .locator(CONNECTOR_PILL_SELECTOR).waitFor({
          state: "visible", timeoutMs: 10000,
        });
      launchedFromCatalog = true;
    }

    stage = "composer";
    let composer = tab.playwright.locator('[id="prompt-textarea"]');
    if ((await composer.count()) !== 1) return failed(stage);
    const initialComposerState = await observeReadyComposer(composer);
    const initialComposerFailure = composerFailureStage(initialComposerState);
    if (initialComposerFailure !== null) return failed(initialComposerFailure);

    stage = "composer_surface";
    let composerSurface = tab.playwright.locator(
      '[data-composer-surface="true"]',
    );
    if ((await composerSurface.count()) !== 1) return failed(stage);

    const readyComposerState = await observeReadyComposer(
      composer,
      composerSurface,
    );
    const readyComposerFailure = composerFailureStage(readyComposerState);
    if (readyComposerFailure !== null) return failed(readyComposerFailure);

    let pill = composerSurface.locator(CONNECTOR_PILL_SELECTOR);
    const initialPillCount = readyComposerState.pillCount;

    if (launchedFromCatalog && initialPillCount !== 1) {
      return failed("catalog_attach");
    }
    let action = launchedFromCatalog ? "attached" : "already_attached";
    let clickResult = launchedFromCatalog ? "verified_catalog_launch" : "not_needed";
    let enteredWorkMode = false;
    // The current normal-Chat composer has no Work/Chat radios. Select this
    // supported layout from positive UI evidence, not after a legacy failure.
    const workCount = await tab.playwright.getByRole("radio", {
      name: "Work", exact: true,
    }).count();
    const chatControlCount = await tab.playwright.getByRole("radio", {
      name: "Chat", exact: true,
    }).count();
    if (initialPillCount === 0 && workCount === 0 && chatControlCount === 0) {
      stage = "work_mode";
      const plus = tab.playwright.locator('[data-testid="composer-plus-btn"]');
      const label = await composer.getAttribute("aria-label");
      if (!["ChatGPT와 채팅", "Chat with ChatGPT"].includes(label) ||
          (await plus.count()) !== 1 ||
          (await plus.getAttribute("aria-haspopup")) !== "menu") {
        return failed(stage);
      }
      stage = "pro_mode";
      if ((await tab.playwright.getByRole("button", {
        name: /^(?:Pro|6 Pro)$/, exact: true,
      }).count()) !== 1) return failed(stage);
      composer = tab.playwright.locator('[id="prompt-textarea"]');
      const beforeMention = await observeReadyComposer(composer, composerSurface);
      const mentionFailure = composerFailureStage(beforeMention);
      if (mentionFailure !== null) return failed(mentionFailure);
      if (beforeMention.pillCount !== 0) return failed("composer_changed");

      // Modern search did not expose this installed development connector in
      // live Chrome. Do not invent a selection or leave a literal @name draft.
      // Return control so the caller can explicitly acquire a fresh-chat lease.
      return failed("connector_picker_unavailable");
    }
    if (initialPillCount === 0 && action !== "attached") {
      stage = "work_mode";
      const work = tab.playwright.getByRole("radio", {
        name: "Work",
        exact: true,
      });
      if ((await work.count()) !== 1) return failed(stage);
      if ((await work.getAttribute("aria-checked")) !== "true") {
        await work.click();
      }
      const workPickerReadyDeadline = Date.now() + PLUGIN_BUTTON_TIMEOUT_MS;
      if (
        (await waitForWorkSelected(
          tab.playwright,
          workPickerReadyDeadline,
        )) === null
      ) {
        return failed(stage);
      }
      enteredWorkMode = true;

      composer = tab.playwright.locator('[id="prompt-textarea"]');
      if (!(await waitForVisible(composer, workPickerReadyDeadline))) {
        return failed("composer_changed");
      }
      if ((await composer.count()) !== 1) return failed("composer_changed");
      composerSurface = tab.playwright.locator(
        '[data-composer-surface="true"]',
      );
      if (!(await waitForVisible(composerSurface, workPickerReadyDeadline))) {
        return failed("composer_surface");
      }
      if ((await composerSurface.count()) !== 1) {
        return failed("composer_surface");
      }
      const workComposerState = await observeReadyComposer(
        composer,
        composerSurface,
      );
      const workComposerFailure = composerFailureStage(workComposerState);
      if (workComposerFailure !== null) return failed(workComposerFailure);

      pill = composerSurface.locator(CONNECTOR_PILL_SELECTOR);
      if (workComposerState.pillCount === 1) {
        clickResult = "verified_without_menu_click";
      } else {
        stage = "plugin_picker";
        const pluginButton = await findUniquePluginButton(
          tab.playwright,
          workPickerReadyDeadline,
        );
        if (pluginButton === null) return failed(stage);
        if ((await pluginButton.getAttribute("aria-haspopup")) !== "menu") {
          return failed(stage);
        }

        composer = tab.playwright.locator('[id="prompt-textarea"]');
        if (!(await waitForVisible(composer, workPickerReadyDeadline))) {
          return failed("composer_changed");
        }
        if ((await composer.count()) !== 1) return failed("composer_changed");
        composerSurface = tab.playwright.locator(
          '[data-composer-surface="true"]',
        );
        if (!(await waitForVisible(composerSurface, workPickerReadyDeadline))) {
          return failed("composer_surface");
        }
        if ((await composerSurface.count()) !== 1) {
          return failed("composer_surface");
        }
        const prePickerComposerState = await observeReadyComposer(
          composer,
          composerSurface,
        );
        const prePickerComposerFailure = composerFailureStage(
          prePickerComposerState,
        );
        if (prePickerComposerFailure !== null) {
          return failed(prePickerComposerFailure);
        }
        pill = composerSurface.locator(CONNECTOR_PILL_SELECTOR);
        if (prePickerComposerState.pillCount === 1) {
          clickResult = "verified_without_menu_click";
        } else {
          await pluginButton.click();

          stage = "connector_match";
          const menuItem = tab.playwright.locator(
            `[data-composer-plugin-impression-id="${CONNECTOR_IMPRESSION_ID}"][role="menuitemcheckbox"]`,
          );
          try {
            await menuItem.waitFor({ state: "visible", timeoutMs: 10000 });
          } catch {
            composerSurface = tab.playwright.locator(
              '[data-composer-surface="true"]',
            );
            if ((await composerSurface.count()) !== 1) {
              return failed("composer_surface");
            }
            pill = composerSurface.locator(CONNECTOR_PILL_SELECTOR);
            if ((await pill.count()) !== 1) return failed(stage);
          }
          if ((await pill.count()) === 1) {
            clickResult = "verified_without_menu_click";
          } else {
            if ((await menuItem.count()) !== 1) return failed(stage);
            const menuItemName = ((await menuItem.textContent()) ?? "")
              .replace(/\s+/g, " ")
              .trim();
            if (menuItemName !== CONNECTOR_NAME) return failed(stage);
            const checked = await menuItem.getAttribute("aria-checked");
            if (checked !== "true" && checked !== "false") return failed(stage);

            composer = tab.playwright.locator('[id="prompt-textarea"]');
            if ((await composer.count()) !== 1) {
              return failed("composer_changed");
            }
            composerSurface = tab.playwright.locator(
              '[data-composer-surface="true"]',
            );
            if ((await composerSurface.count()) !== 1) {
              return failed("composer_surface");
            }
            const preAttachComposerState = await observeReadyComposer(
              composer,
              composerSurface,
            );
            const preAttachComposerFailure = composerFailureStage(
              preAttachComposerState,
            );
            if (preAttachComposerFailure !== null) {
              return failed(preAttachComposerFailure);
            }
            pill = composerSurface.locator(CONNECTOR_PILL_SELECTOR);
            if (preAttachComposerState.pillCount === 1) {
              clickResult = "verified_without_menu_click";
            } else {
              stage = "connector_attach";
              if (checked === "false") {
                clickResult = "completed";
                try {
                  await menuItem.click();
                } catch {
                  clickResult = "error_pending_verification";
                }
              } else {
                clickResult = "verified_without_menu_click";
              }
              composerSurface = tab.playwright.locator(
                '[data-composer-surface="true"]',
              );
              if ((await composerSurface.count()) !== 1) {
                return failed("composer_surface");
              }
              pill = composerSurface.locator(CONNECTOR_PILL_SELECTOR);
              await pill.waitFor({ state: "visible", timeoutMs: 10000 });
            }
          }
        }
      }
      if ((await pill.count()) !== 1) return failed(stage);
      if (clickResult === "error_pending_verification") {
        clickResult = "verified_after_error";
      }
      action = "attached";
    }

    stage = "chat_mode";
    const chat = tab.playwright.getByRole("radio", {
      name: "Chat",
      exact: true,
    });
    const chatCount = await chat.count();
    if (chatCount > 1) return failed(stage);
    if (chatCount === 0) {
      const work = tab.playwright.getByRole("radio", {
        name: "Work",
        exact: true,
      });
      if (enteredWorkMode || (await work.count()) !== 0) {
        return failed(stage);
      }
      if (launchedFromCatalog && !["ChatGPT와 채팅", "Chat with ChatGPT"].includes(
        await composer.getAttribute("aria-label"),
      )) return failed(stage);
    } else {
      if ((await chat.getAttribute("aria-checked")) !== "true") {
        await chat.click();
      }
      if ((await chat.getAttribute("aria-checked")) !== "true") {
        return failed(stage);
      }
    }
    composerSurface = tab.playwright.locator(
      '[data-composer-surface="true"]',
    );
    if ((await composerSurface.count()) !== 1) {
      return failed("composer_surface");
    }
    pill = composerSurface.locator(CONNECTOR_PILL_SELECTOR);
    if ((await pill.count()) !== 1) return failed("connector_after_chat");

    stage = "pro_mode";
    const pro = tab.playwright.getByRole("button", {
      name: /^(?:Pro|6 Pro)$/,
      exact: true,
    });
    if ((await pro.count()) !== 1) return failed(stage);

    composer = tab.playwright.locator('[id="prompt-textarea"]');
    if ((await composer.count()) !== 1) return failed("composer_changed");
    const finalComposerState = await observeReadyComposer(
      composer,
      composerSurface,
    );
    const finalComposerFailure = composerFailureStage(finalComposerState);
    if (finalComposerFailure !== null) return failed(finalComposerFailure);
    if (finalComposerState.pillCount !== 1) {
      return failed("connector_after_chat");
    }

    return {
      protocol: PROTOCOL,
      browser_type: "chrome",
      status: "verified",
      connector_name: CONNECTOR_NAME,
      connector_path: CONNECTOR_PATH,
      chat_mode: "chat",
      pro_mode: true,
      action,
      click_result: clickResult,
    };
  } catch {
    return failed(stage);
  }
}
