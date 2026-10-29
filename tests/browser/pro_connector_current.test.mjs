import assert from 'node:assert/strict';
import test from 'node:test';
import { pathToFileURL } from 'node:url';

const source = process.env.PRO_CONNECTOR_TEST_SOURCE
  ? pathToFileURL(process.env.PRO_CONNECTOR_TEST_SOURCE).href
  : new URL('../../plugins/codex-discord-remote/skills/ask-chatgpt-pro/scripts/pro_connector_control.mjs', import.meta.url).href;
const { prepareProConnector } = await import(source);
const name = 'Simdorei Local Project Oauth';
const path = '/plugins/plugin_asdk_app_6a6ae90be0a08191b877eddba93b631c';
const editor = '[contenteditable="true"][role="textbox"][data-composer-markdown]';
const surface = 'form[data-thread-find-composer="true"]';
const pill = `a[href="${path}"], a[href^="${path}?"], a[href^="${path}#"]`;
const mentionName = 'dev-6a6ae90be0a08191b877eddba93b631c';
const mentionPath = `plugin://${mentionName}@openai-curated-remote`;
const chip = `span[contenteditable="false"][plugin-mention-name="${mentionName}"][plugin-mention-display-name="${name}"][plugin-mention-path="${mentionPath}"][data-prompt-link-href="${mentionPath}"]`;
const appName = 'simdorei-local-project-oauth';
const appPath = 'app://asdk_app_6a6ae90be0a08191b877eddba93b631c';
const appChip = `span[contenteditable="false"][app-mention-name="${appName}"][app-mention-display-name="${name}"][app-mention-path="${appPath}"][data-prompt-link-href="${appPath}"]`;
const currentPill = pill + ', ' + chip + ', ' + appChip;
const model = 'button[data-composer-navigation-target="reasoning"]:is([data-codex-intelligence-trigger="true"], [aria-label="ChatGPT 모델 선택"])';
const plus = 'button[data-composer-navigation-target="add-context"]';

// A DOM-shaped adapter for the observed current composer. Unknown selectors fail
// closed; a bare connector name never behaves like an exact composer link.
function fixture(config = {}) {
  const state = {
    catalog: true, attached: false, chat: false, draft: '', chip: true,
    mentionName, mentionPath, mentionHref: mentionPath, mentionLabel: name,
    mentionEditable: 'false', appName, appPath, appHref: appPath, appLabel: name,
    appEditable: 'false', ...config,
  };
  const exactChip = () => state.mentionName === mentionName &&
    state.mentionPath === mentionPath && state.mentionHref === mentionPath &&
    state.mentionLabel === name && state.mentionEditable === 'false';
  const exactAppChip = () => state.appName === appName && state.appPath === appPath &&
    state.appHref === appPath && state.appLabel === name && state.appEditable === 'false';
  const calls = [];
  const activations = [];
  const attachPicker = () => {
    state.attached = state.attachOnPicker !== false;
    state.menuOpen = false;
    if (state.foreignOnPicker) state.mentionName = 'another-plugin';
    if (state.draftOnPicker) state.draft = 'concurrent user draft';
    if (state.navigateOnPicker) state.resultUrl = 'https://chatgpt.com/c/other';
    if (state.throwAfterPicker) throw new Error('detached after activation');
  };
  const attachResearch = () => {
    state.attached = true;
    state.menuOpen = false;
    state.appChip = true;
    state.appName = 'app';
    state.appPath = 'app://connector_openai_deep_research';
    state.appHref = state.appPath;
    state.appLabel = 'Deep research';
  };
  const loc = kind => ({
    count: async () => {
      if (kind === 'heading') return state.catalog ? (state.headingCount ?? 1) : 0;
      if (kind === 'use') return state.catalog ? (state.useCount ?? 1) : 0;
      if (kind === 'missing' || state.catalog) return 0;
      if (kind === 'editor') return state.editorCount ?? 1;
      if (kind === 'surface') return state.surfaceCount ?? 1;
      if (kind === 'pill') return state.attached && (state.appChip ? exactAppChip() : !state.chip || exactChip()) ? (state.pillCount ?? 1) : 0;
      if (kind === 'chip') return state.attached && !state.appChip && state.chip && exactChip() ? (state.pillCount ?? 1) : 0;
      if (kind === 'app-chip') return state.attached && state.appChip && exactAppChip() ? (state.pillCount ?? 1) : 0;
      if (kind === 'anchor') return state.attached && !state.appChip && !state.chip ? (state.pillCount ?? 1) : 0;
      if (kind === 'all-chips') return (state.attached && !state.appChip && state.chip ? (state.pillCount ?? 1) : 0) + (state.foreignChips ?? 0);
      if (kind === 'all-app-chips') return (state.attached && state.appChip ? (state.pillCount ?? 1) : 0) + (state.foreignAppChips ?? 0);
      if (kind === 'all-anchors') return (state.attached && !state.appChip && !state.chip ? (state.pillCount ?? 1) : 0) + (state.foreignAnchors ?? 0);
      if (kind === 'model') {
        if (state.draftOnModel) state.draft = 'concurrent user draft';
        return state.modelCount ?? 1;
      }
      if (kind === 'pro') return state.proCount ?? ((state.modelText ?? 'Pro') === 'Pro' ? 1 : 0);
      if (kind === 'chat') return state.chatCount ?? (state.plainChat ? 0 : 1);
      if (kind === 'work') return state.workCount ?? (state.plainChat ? 0 : 1);
      if (kind === 'modes') return state.modesCount ?? (state.plainChat ? 0 : 1);
      if (kind === 'plus') return state.picker ? (state.plusCount ?? 1) : 0;
      if (kind === 'menu') return state.menuOpen && state.picker ? (state.menuCount ?? 1) : 0;
      if (kind === 'work-radio') return state.workRadioCount ?? 0;
      return 0;
    },
    locator: selector => loc(selector === currentPill ? 'pill' : selector === chip ? 'chip' : selector === appChip ? 'app-chip' : selector === pill ? 'anchor' : selector === 'span[plugin-mention-path]' ? 'all-chips' : selector === 'span[app-mention-path]' ? 'all-app-chips' : selector === 'a[href^="/plugins/"]' ? 'all-anchors' : selector === editor ? 'editor' : selector === model ? 'model' : selector === plus ? 'plus' : 'missing'),
    getByRole: (role, options) => loc(role === 'button' && options.exact && ['Chat', 'Work'].includes(options.name) ? options.name.toLowerCase() : 'missing'),
    getByText: (text, options) => loc(kind === 'model' && text === 'Pro' && options.exact ? 'pro' : 'missing'),
    textContent: async () => kind === 'editor' ? state.draft + (state.attached ? (state.appChip ? state.appLabel : name) : '') : kind === 'pill' ? name : '',
    innerText: async () => kind === 'model' ? (state.modelText ?? 'Pro') : '',
    getAttribute: async attribute => {
      if (attribute === 'aria-haspopup' && kind === 'model') return 'menu';
      if (attribute === 'aria-pressed' && kind === 'chat') return state.chat ? 'true' : 'false';
      if (attribute === 'aria-pressed' && kind === 'work') return state.chat ? 'false' : 'true';
      if (attribute === 'aria-expanded' && kind === 'plus') return state.menuOpen ? 'true' : 'false';
      if (attribute === 'type' && kind === 'menu') return 'button';
      if (attribute === 'data-list-navigation-item' && kind === 'menu') return Object.hasOwn(state, 'menuNavigation') ? state.menuNavigation : 'true';
      if (attribute === 'aria-label' && kind === 'editor' && state.plainChat) return state.editorLabel ?? 'ChatGPT에게 물어보세요';
      if (attribute === 'data-chatgpt-composer' && kind === 'surface' && state.plainChat) return '';
      if (attribute === 'data-composer-placement' && kind === 'surface' && state.plainChat) return state.placement ?? 'thread';
      return null;
    },
    press: state.keyboardUnavailable ? undefined : async (key, options) => {
      assert.equal(kind, 'menu');
      assert.equal(key, 'Enter');
      assert.deepEqual(options, { timeoutMs: 10000 });
      calls.push(kind);
      activations.push('keyboard');
      if (state.keyboardFailsBeforeActivation) throw new Error('focus or key failed');
      if (state.keyboardWrongApp) attachResearch();
      else attachPicker();
    },
    click: async () => {
      calls.push(kind);
      if (kind === 'use') {
        state.catalog = false;
        state.attached = state.attachOnLaunch !== false;
      }
      if (kind === 'chat' && state.switchChat !== false) {
        state.chat = true;
        if (state.losePillOnChat) state.attached = false;
      }
      if (kind === 'plus') {
        state.menuOpen = true;
        if (state.draftOnPlus) state.draft = 'concurrent user draft';
        if (state.navigateOnPlus) state.resultUrl = 'https://chatgpt.com/c/other';
      }
      if (kind === 'menu') {
        activations.push('pointer');
        if (state.pointerMisroutes) attachResearch();
        else attachPicker();
      }
    },
    waitFor: async () => {
      if ((await loc(kind).count()) !== 1) throw new Error('not uniquely visible');
    },
  });
  return {
    calls,
    activations,
    state,
    tab: {
      url: async () => state.catalog ? 'https://chatgpt.com' + (state.catalogPath ?? path) : (state.resultUrl ?? 'https://chatgpt.com/'),
      ax: config.axUnavailable ? undefined : {
        get: async (mode, options) => {
          assert.equal(mode, 'state');
          assert.deepEqual(options, { disableDiffing: true });
          if (state.axSnapshotFailure) throw new Error('accessibility unavailable');
          if (state.draftOnAxSnapshot) state.draft = 'concurrent user draft';
          if (state.navigateOnAxSnapshot) state.resultUrl = 'https://chatgpt.com/c/other';
          return state.axState ?? `0 AXWebArea\n\t1487 button Deep research\n\t1499 button ${name} ${name}\n\t1500 text ${name}`;
        },
        click: async index => {
          assert.equal(index, 1499);
          calls.push('menu');
          activations.push('accessibility');
          if (state.axFailsBeforeActivation) throw new Error('activation failed');
          if (state.axWrongApp) attachResearch();
          else attachPicker();
        },
      },
      playwright: {
        locator: selector => loc(selector === editor ? 'editor' : selector === surface ? 'surface' : 'missing'),
        getByRole: (role, options) => {
          if (!options.exact) return loc('missing');
          if (role === 'heading' && options.name === name + ' 내 클라우드 플러그인') return loc('heading');
          if (role === 'button' && options.name === '채팅에서 사용해 보기') return loc('use');
          if (role === 'group' && options.name === '작성기 모드') return loc('modes');
          if (role === 'button' && options.name === `${name} ${name}`) return loc('menu');
          if (role === 'radio' && options.name === 'Work') return loc('work-radio');
          return loc('missing');
        },
      },
    },
  };
}

async function run(config) {
  const f = fixture(config);
  return { result: await prepareProConnector({ proConversationTab: f.tab }), calls: f.calls };
}

test('current catalog verifies exact attachment only after Chat and Pro', async () => {
  const { result, calls } = await run();
  assert.equal(result.status, 'verified');
  assert.equal(result.connector_path, path);
  assert.equal(result.pro_mode, true);
  assert.equal(result.chat_mode, 'chat');
  assert.equal(result.click_result, 'verified_catalog_launch');
  assert.deepEqual(calls, ['use', 'chat']);
});

test('current existing exact pill is verified without another attachment', async () => {
  const { result, calls } = await run({ catalog: false, attached: true, chat: true });
  assert.equal(result.status, 'verified');
  assert.equal(result.action, 'already_attached');
  assert.deepEqual(calls, []);
});

test('current exact non-editable mention is verified without reattaching', async () => {
  const { result, calls } = await run({ catalog: false, attached: true, chat: true, chip: true });
  assert.equal(result.status, 'verified');
  assert.equal(result.action, 'already_attached');
  assert.deepEqual(calls, []);
});

test('current exact legacy link remains supported', async () => {
  const { result } = await run({ catalog: false, attached: true, chat: true, chip: false });
  assert.equal(result.status, 'verified');
});

test('current mention requires the complete exact identity and non-editable node', async () => {
  for (const change of [
    { mentionName: mentionName + '-other' }, { mentionPath: mentionPath + '-other' },
    { mentionHref: 'plugin://another' }, { mentionLabel: name + ' extra' },
    { mentionEditable: 'true' }, { foreignChips: 1 }, { foreignAnchors: 1 },
  ]) {
    const { result, calls } = await run({ catalog: false, attached: true, chat: true, ...change });
    assert.equal(result.status, 'failed', JSON.stringify(change));
    assert.deepEqual(calls, []);
  }
});

test('current catalog rejects wrong identity and ambiguous controls before click', async () => {
  for (const config of [{ catalogPath: '/plugins/unrelated' }, { headingCount: 2 }, { useCount: 2 }]) {
    const { result, calls } = await run(config);
    assert.equal(result.status, 'failed');
    assert.deepEqual(calls, []);
  }
});

test('current missing pill and name-only mentions cannot become attachment proof', async () => {
  for (const config of [{ attachOnLaunch: false }, { catalog: false, draft: '@' + name }]) {
    const { result } = await run(config);
    assert.equal(result.status, 'failed');
  }
});

test('current draft remains protected before and during verification', async () => {
  for (const config of [
    { catalog: false, attached: true, chat: true, draft: 'user draft' },
    { catalog: false, attached: true, chat: true, draft: 'ChatGPT에게 물어보세요' },
    { catalog: false, attached: true, chat: true, draftOnModel: true },
  ]) {
    const { result, calls } = await run(config);
    assert.equal(result.status, 'failed');
    assert.equal(result.failed_stage, 'composer_not_empty');
    assert.deepEqual(calls, []);
  }
});

test('current mode model and composer ambiguity fail closed', async () => {
  for (const config of [
    { switchChat: false }, { losePillOnChat: true }, { modelText: 'Thinking' },
    { modelText: 'Pro settings' }, { modelCount: 2 }, { chatCount: 2 },
    { modesCount: 0 }, { editorCount: 2 }, { surfaceCount: 2 }, { pillCount: 2 },
    { resultUrl: 'https://unrelated.invalid/' },
  ]) {
    const { result } = await run(config);
    assert.equal(result.status, 'failed', JSON.stringify(config));
  }
});

const plainPicker = {
  catalog: false, attached: false, plainChat: true, picker: true,
  resultUrl: 'https://chatgpt.com/c/6ab7958e-82a0-83e8-b5b4-f056a0610259',
};

test('Korean reasoning label preserves exact Pro verification before and after attachment', async () => {
  for (const attached of [false, true]) {
    const { result } = await run({ ...plainPicker, attached, modelText: '추론 수준\nPro', proCount: 1 });
    assert.equal(result.status, 'verified');
  }
  for (const config of [
    { modelText: '추론 수준\nThinking', proCount: 1 },
    { modelText: '추론 수준\nPro settings', proCount: 1 },
    { modelText: '추론 수준\nPro', proCount: 0 },
    { modelText: '추론 수준\nPro', proCount: 2 },
  ]) {
    const { result } = await run({ ...plainPicker, ...config });
    assert.equal(result.status, 'failed');
  }
});

test('current thread picker attaches exact chip without creating another conversation', async () => {
  const f = fixture(plainPicker);
  const originalUrl = await f.tab.url();
  const result = await prepareProConnector({ proConversationTab: f.tab });
  assert.equal(result.status, 'verified');
  assert.equal(result.action, 'attached');
  assert.equal(result.click_result, 'verified_composer_picker');
  assert.equal(result.chat_mode, 'chat');
  assert.equal(result.pro_mode, true);
  assert.equal(await f.tab.url(), originalUrl);
  assert.deepEqual(f.calls, ['plus', 'menu']);
  assert.equal((await prepareProConnector({ proConversationTab: f.tab })).action, 'already_attached');
  assert.deepEqual(f.calls, ['plus', 'menu']);
});

test('current plain Chat verifies an existing exact chip without mode toggles', async () => {
  const { result, calls } = await run({ ...plainPicker, attached: true });
  assert.equal(result.status, 'verified');
  assert.deepEqual(calls, []);
});

test('current picker preserves the existing explicit Work to Chat transition', async () => {
  const { result, calls } = await run({ ...plainPicker, plainChat: false });
  assert.equal(result.status, 'verified');
  assert.deepEqual(calls, ['plus', 'menu', 'chat']);
});

test('current picker never toggles an already open menu or repeats an uncertain click', async () => {
  for (const config of [{ menuOpen: true }, { throwAfterPicker: true }]) {
    const { result, calls } = await run({ ...plainPicker, ...config });
    assert.equal(result.status, 'verified');
    assert.equal(calls.filter(x => x === 'menu').length, 1);
    assert.equal(calls.filter(x => x === 'plus').length, config.menuOpen ? 0 : 1);
  }
});

test('current picker does not accept missing duplicate or malformed menu entries', async () => {
  for (const config of [{ menuCount: 0 }, { menuCount: 2 }, { menuNavigation: null }]) {
    const { result, calls } = await run({ ...plainPicker, ...config });
    assert.equal(result.status, 'failed');
    assert.equal(calls.includes('menu'), false);
  }
});

test('current picker name is never proof of the resulting plugin identity', async () => {
  for (const config of [{ foreignOnPicker: true }, { attachOnPicker: false }, { pillCount: 2 }]) {
    const { result, calls } = await run({ ...plainPicker, ...config });
    assert.equal(result.status, 'failed');
    assert.deepEqual(calls, ['plus', 'menu']);
  }
});

test('current picker preserves drafts before open before attachment and after attachment', async () => {
  for (const [config, expected] of [
    [{ draft: 'user draft' }, []],
    [{ draftOnModel: true }, []],
    [{ draftOnPlus: true }, ['plus']],
    [{ draftOnPicker: true }, ['plus', 'menu']],
  ]) {
    const { result, calls } = await run({ ...plainPicker, ...config });
    assert.equal(result.status, 'failed');
    assert.equal(result.failed_stage, 'composer_not_empty');
    assert.deepEqual(calls, expected);
  }
});

test('current picker requires Pro and unique controls before attachment', async () => {
  for (const config of [{ modelText: 'Thinking' }, { modelCount: 2 }, { plusCount: 2 }]) {
    const { result, calls } = await run({ ...plainPicker, ...config });
    assert.equal(result.status, 'failed');
    assert.deepEqual(calls, []);
  }
});

test('absence of mode toggles alone is not normal Chat evidence', async () => {
  for (const config of [
    { editorLabel: 'Work' }, { placement: 'unknown' }, { workCount: 1 },
    { workRadioCount: 1 }, { modesCount: 2 },
  ]) {
    const { result } = await run({ ...plainPicker, attached: true, ...config });
    assert.equal(result.status, 'failed');
    assert.equal(result.failed_stage, 'chat_mode');
  }
});

test('current picker never verifies after conversation navigation', async () => {
  for (const [config, expected] of [
    [{ navigateOnPlus: true }, ['plus']],
    [{ navigateOnPicker: true }, ['plus', 'menu']],
  ]) {
    const { result, calls } = await run({ ...plainPicker, ...config });
    assert.equal(result.status, 'failed');
    assert.equal(result.failed_stage, 'conversation_url');
    assert.deepEqual(calls, expected);
  }
});

test('current exact app chip is accepted without selecting or changing models again', async () => {
  const { result, calls } = await run({ ...plainPicker, attached: true, appChip: true });
  assert.equal(result.status, 'verified');
  assert.equal(result.action, 'already_attached');
  assert.deepEqual(calls, []);
});

test('current picker verifies the observed app attachment identity', async () => {
  const { result, calls } = await run({ ...plainPicker, appChip: true });
  assert.equal(result.status, 'verified');
  assert.deepEqual(calls, ['plus', 'menu']);
});

test('current app chip must have exact name display path href and non-editable state', async () => {
  for (const change of [
    { appName: appName + '-other' }, { appPath: appPath + '-other' },
    { appHref: 'app://another' }, { appLabel: name + ' extra' },
    { appEditable: 'true' }, { foreignAppChips: 1 }, { foreignChips: 1 },
    { foreignAnchors: 1 }, { pillCount: 2 },
  ]) {
    const { result, calls } = await run({ ...plainPicker, attached: true, appChip: true, ...change });
    assert.equal(result.status, 'failed', JSON.stringify(change));
    assert.deepEqual(calls, []);
  }
});

test('current app chips cannot hide another app behind a valid plugin chip', async () => {
  const { result, calls } = await run({ ...plainPicker, attached: true, foreignAppChips: 1 });
  assert.equal(result.status, 'failed');
  assert.deepEqual(calls, []);
});

test('current picker avoids both observed pointer misrouting and locator key failure', async () => {
  const f = fixture({ ...plainPicker, appChip: true, pointerMisroutes: true, keyboardFailsBeforeActivation: true });
  const originalUrl = await f.tab.url();
  const result = await prepareProConnector({ proConversationTab: f.tab });
  assert.equal(result.status, 'verified');
  assert.equal(f.state.appPath, appPath);
  assert.equal(await f.tab.url(), originalUrl);
  assert.deepEqual(f.activations, ['accessibility']);
});

test('current accessibility activation is observed once after an uncertain error', async () => {
  const f = fixture({ ...plainPicker, appChip: true, throwAfterPicker: true });
  const result = await prepareProConnector({ proConversationTab: f.tab });
  assert.equal(result.status, 'verified');
  assert.equal(result.click_result, 'verified_after_error');
  assert.deepEqual(f.activations, ['accessibility']);
});

test('current accessibility failure never falls back to pointer or keyboard', async () => {
  const f = fixture({ ...plainPicker, axFailsBeforeActivation: true });
  const result = await prepareProConnector({ proConversationTab: f.tab });
  assert.equal(result.status, 'failed');
  assert.equal(result.failed_stage, 'connector_attach');
  assert.equal(f.state.attached, false);
  assert.deepEqual(f.activations, ['accessibility']);
});

test('current accessibility activation rejects a foreign resulting app identity', async () => {
  const f = fixture({ ...plainPicker, appChip: true, axWrongApp: true });
  const result = await prepareProConnector({ proConversationTab: f.tab });
  assert.equal(result.status, 'failed');
  assert.deepEqual(f.activations, ['accessibility']);
});

test('current inaccessible picker cannot trigger pointer or global-key fallback', async () => {
  for (const config of [{ axUnavailable: true }, { axSnapshotFailure: true }]) {
    const f = fixture({ ...plainPicker, ...config });
    const result = await prepareProConnector({ proConversationTab: f.tab });
    assert.equal(result.status, 'failed');
    assert.equal(result.failed_stage, 'connector_match');
    assert.deepEqual(f.activations, []);
    assert.deepEqual(f.calls, ['plus']);
  }
});

test('current accessibility selection requires exactly one safe exact button index', async () => {
  for (const axState of [
    '',
    '1499 text ' + name + ' ' + name,
    '1499 button ' + name + ' ' + name + ' extra',
    '1499 button ' + name + ' ' + name + '\n1502 button ' + name + ' ' + name,
    '9007199254740992 button ' + name + ' ' + name,
  ]) {
    const f = fixture({ ...plainPicker, axState });
    const result = await prepareProConnector({ proConversationTab: f.tab });
    assert.equal(result.status, 'failed', axState);
    assert.deepEqual(f.activations, []);
  }
});

test('current accessibility snapshot cannot hide a concurrent draft or navigation', async () => {
  for (const [config, stage] of [
    [{ draftOnAxSnapshot: true }, 'composer_not_empty'],
    [{ navigateOnAxSnapshot: true }, 'conversation_url'],
  ]) {
    const f = fixture({ ...plainPicker, ...config });
    const result = await prepareProConnector({ proConversationTab: f.tab });
    assert.equal(result.status, 'failed');
    assert.equal(result.failed_stage, stage);
    assert.deepEqual(f.activations, []);
  }
});

