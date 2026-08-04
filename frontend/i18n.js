// i18n: string tables + helpers. Static DOM strings carry data-i18n / data-i18n-ph attributes and
// are swept by applyStatic(); dynamic strings go through t(). Manifest labels are either plain
// strings or {lang: text} objects — resolved by mlabel().

const I18N = {
  en: {
    "status.notChecked": "Not checked",
    "status.checkHint": "Check to read the manifest.",
    "status.working": "Working…",
    "status.upToDate": "Up to date",
    "status.updateAvail": "Update available",
    "status.notInstalled": "Not installed",
    "status.repair": "Needs repair",
    "detail.repair": "{version} · files intact, install record missing",
    "status.error": "Error",
    "status.offline": "Offline",
    "status.noAccess": "No access",
    "status.tooOld": "Launcher outdated",
    "status.gameLocked": "Game is running",
    "err.network": "Could not reach the update server — check your connection.",
    "err.auth": "The source repo refused access — an access token may be required.",
    "err.tooOld": "This release needs a newer launcher.",
    "err.gameRunning": "Close Dota 2 and try again.",
    "status.launched": "Launched",
    "status.ingame": "In game",
    "detail.ingame": "Dota 2 is running.",
    "status.saved": "Settings saved",
    "detail.reading": "Reading the manifest…",
    "detail.installing": "Installing…",
    "detail.dl": "Downloading files… {i}/{n} done",
    "detail.reverting": "Reverting to stock…",
    "detail.launched": "Dota 2 is starting.",
    "detail.okMeta": "{version}",
    "detail.changes": "{version} · {n} file(s) to change",

    "files.title": "Managed files",
    "files.empty": "Nothing read yet.",
    "files.allCurrent": "all current",
    "files.toChange": "{n} to change",
    "fstate.ok": "current",
    "fstate.update": "update",
    "fstate.install": "new",
    "fstate.remove": "remove",

    "btn.check": "Check for updates",
    "btn.play": "Play",
    "btn.ingame": "In game",
    "btn.install": "Install",
    "btn.update": "Update",
    "btn.repair": "Repair",
    "btn.clearToken": "Clear",
    "btn.save": "Save changes",
    "btn.back": "Back",
    "btn.browse": "Browse",
    "btn.autofind": "Autofind",
    "btn.cancel": "Cancel",
    "btn.close": "Close",
    "btn.continue": "Continue",
    "btn.stop": "Stop",

    "link.whatsnew": "What's new",
    "link.uninstall": "Uninstall",
    "link.customize": "Customize",

    "head.settings": "SETTINGS",
    "head.settingsTip": "Settings",
    "head.whatsnew": "WHAT'S NEW",
    "head.options": "CUSTOMIZATION",
    "head.autoexec": "AUTOEXEC.CFG",
    "head.setup": "SETUP",

    "set.language": "Language",
    "set.gameFolder": "Game folder",
    "set.gameHint": "The folder that contains the folder `game`.",
    "set.launch": "Launch options",
    "set.launchHint": "Always passed. Add your own after.",
    "set.launchPh": "additional options",
    "set.renderer": "Renderer",
    "set.autoexec": "Config",
    "set.autoexecBtn": "Edit autoexec.cfg",
    "set.advanced": "Advanced",
    "set.repo": "Source repo",
    "set.repoHint": "Where releases are pulled from.",
    "set.token": "Access token",
    "set.tokenHint": "Only for a private source repo.",
    "ph.gameAuto": "auto — the updater's own folder",
    "ph.tokenEmpty": "leave empty for the public repo",
    "ph.tokenSaved": "•••••••• saved — leave blank to keep",
    "ph.tokenCleared": "will be removed on save",

    "setup.title": "Locate the game",
    "setup.text": "Point the updater at your Dota 2 6.88 folder — the one that contains game\\.",
    "setup.found": "Found folders",
    "setup.none": "Nothing found.",
    "setup.use": "Use",
    "setup.build": "build {v}",

    "af.title": "Automatic search",
    "af.warning": "Scans all drives for game folders. Depending on your hardware this can take a long time, and results may be inaccurate.",
    "af.scanning": "Scanning…",
    "af.scanned": "{n} folders scanned",

    "opt.hint": "Changes apply on the next install or update.",
    "ae.unsaved": "unsaved changes",
    "ae.lossy": "This file is not UTF-8 — shown read-only so saving can't corrupt it.",
    "wn.loading": "Loading earlier versions…",
    "wn.none": "No release notes yet.",

    "cf.uninstallTitle": "Uninstall",
    "cf.uninstallText": "Revert the game to stock? Every file Phoenix placed will be removed.",
    "cf.uninstallConfirm": "Uninstall",
    "cf.discardTitle": "Discard changes",
    "cf.discardText": "autoexec.cfg has unsaved changes. Discard them?",
    "cf.discardSettingsText": "Settings have unsaved changes. Discard them?",
    "cf.discardConfirm": "Discard",
    "cf.quitTitle": "Quit",
    "cf.quitText": "An operation is still running. Quit anyway? An interrupted download will resume next time.",
    "cf.quitConfirm": "Quit",
  },
  ru: {
    "status.notChecked": "Не проверено",
    "status.checkHint": "Проверьте, чтобы прочитать манифест.",
    "status.working": "Работаю…",
    "status.upToDate": "Актуально",
    "status.updateAvail": "Есть обновление",
    "status.notInstalled": "Не установлено",
    "status.repair": "Нужно восстановление",
    "detail.repair": "{version} · файлы целы, запись об установке отсутствует",
    "status.error": "Ошибка",
    "status.offline": "Нет сети",
    "status.noAccess": "Нет доступа",
    "status.tooOld": "Лаунчер устарел",
    "status.gameLocked": "Игра запущена",
    "err.network": "Не удалось связаться с сервером обновлений — проверьте подключение.",
    "err.auth": "Репозиторий-источник отказал в доступе — возможно, нужен токен доступа.",
    "err.tooOld": "Для этого релиза нужен более новый лаунчер.",
    "err.gameRunning": "Закройте Dota 2 и попробуйте снова.",
    "status.launched": "Запущено",
    "status.ingame": "В игре",
    "detail.ingame": "Dota 2 запущена.",
    "status.saved": "Настройки сохранены",
    "detail.reading": "Читаю манифест…",
    "detail.installing": "Устанавливаю…",
    "detail.dl": "Скачиваю файлы… готово {i}/{n}",
    "detail.reverting": "Возвращаю к исходному…",
    "detail.launched": "Dota 2 запускается.",
    "detail.okMeta": "{version}",
    "detail.changes": "{version} · файлов к изменению: {n}",

    "files.title": "Управляемые файлы",
    "files.empty": "Ещё ничего не прочитано.",
    "files.allCurrent": "всё актуально",
    "files.toChange": "к изменению: {n}",
    "fstate.ok": "актуален",
    "fstate.update": "обновить",
    "fstate.install": "новый",
    "fstate.remove": "удалить",

    "btn.check": "Проверить обновления",
    "btn.play": "Играть",
    "btn.ingame": "В игре",
    "btn.install": "Установить",
    "btn.update": "Обновить",
    "btn.repair": "Восстановить",
    "btn.clearToken": "Убрать",
    "btn.save": "Сохранить",
    "btn.back": "Назад",
    "btn.browse": "Обзор",
    "btn.autofind": "Автопоиск",
    "btn.cancel": "Отмена",
    "btn.close": "Закрыть",
    "btn.continue": "Продолжить",
    "btn.stop": "Остановить",

    "link.whatsnew": "Что нового",
    "link.uninstall": "Удалить",
    "link.customize": "Кастомизация",

    "head.settings": "НАСТРОЙКИ",
    "head.settingsTip": "Настройки",
    "head.whatsnew": "ЧТО НОВОГО",
    "head.options": "КАСТОМИЗАЦИЯ",
    "head.autoexec": "AUTOEXEC.CFG",
    "head.setup": "УСТАНОВКА",

    "set.language": "Язык",
    "set.gameFolder": "Папка игры",
    "set.gameHint": "Папка, содержащая папку `game`.",
    "set.launch": "Параметры запуска",
    "set.launchHint": "Передаются всегда. Свои — после них.",
    "set.launchPh": "дополнительные параметры",
    "set.renderer": "Рендер",
    "set.autoexec": "Конфиг",
    "set.autoexecBtn": "Править autoexec.cfg",
    "set.advanced": "Дополнительно",
    "set.repo": "Репозиторий-источник",
    "set.repoHint": "Откуда берутся релизы.",
    "set.token": "Токен доступа",
    "set.tokenHint": "Только для приватного репозитория.",
    "ph.gameAuto": "авто — папка самого апдейтера",
    "ph.tokenEmpty": "оставьте пустым для публичного репозитория",
    "ph.tokenSaved": "•••••••• сохранён — оставьте пустым, чтобы не менять",
    "ph.tokenCleared": "будет удалён при сохранении",

    "setup.title": "Где игра?",
    "setup.text": "Укажите папку Dota 2 6.88 — ту, что содержит game\\.",
    "setup.found": "Найденные папки",
    "setup.none": "Ничего не найдено.",
    "setup.use": "Выбрать",
    "setup.build": "сборка {v}",

    "af.title": "Автоматический поиск",
    "af.warning": "Сканирует все диски в поисках папок игры. В зависимости от железа это может занять много времени, а результаты могут быть неточными.",
    "af.scanning": "Сканирую…",
    "af.scanned": "просмотрено папок: {n}",

    "opt.hint": "Изменения применятся при следующей установке или обновлении.",
    "ae.unsaved": "есть несохранённые изменения",
    "ae.lossy": "Файл не в UTF-8 — открыт только для чтения, чтобы сохранение его не испортило.",
    "wn.loading": "Загружаю предыдущие версии…",
    "wn.none": "Примечания к выпускам отсутствуют.",

    "cf.uninstallTitle": "Удаление",
    "cf.uninstallText": "Вернуть игру к исходному состоянию? Все файлы, установленные Phoenix, будут удалены.",
    "cf.uninstallConfirm": "Удалить",
    "cf.discardTitle": "Отменить изменения",
    "cf.discardText": "В autoexec.cfg есть несохранённые изменения. Отбросить их?",
    "cf.discardSettingsText": "В настройках есть несохранённые изменения. Отбросить их?",
    "cf.discardConfirm": "Отбросить",
    "cf.quitTitle": "Выход",
    "cf.quitText": "Операция ещё выполняется. Всё равно выйти? Прерванная загрузка продолжится в следующий раз.",
    "cf.quitConfirm": "Выйти",
  },
};

let LANG = "en";

function detectLang() {
  return (navigator.language || "").toLowerCase().startsWith("ru") ? "ru" : "en";
}

function setLang(l) {
  LANG = I18N[l] ? l : "en";
  document.documentElement.lang = LANG;
}

function t(key, vars) {
  let s = (I18N[LANG] && I18N[LANG][key]) ?? I18N.en[key] ?? key;
  if (vars) for (const [k, v] of Object.entries(vars)) s = s.replaceAll(`{${k}}`, String(v));
  return s;
}

/// Resolve a manifest label (plain string or {lang: text}).
function mlabel(v) {
  if (v == null) return "";
  if (typeof v === "string") return v;
  return v[LANG] ?? v.en ?? Object.values(v)[0] ?? "";
}

/// Backtick-wrapped runs in a static string become inline <code> chips (built via DOM nodes —
/// no HTML ever gets parsed). Only fully balanced backticks qualify (odd part count after the
/// split); any unbalanced string renders as-is, backticks included, rather than silently
/// dropping characters.
function inlineCode(s) {
  const parts = s.split("`");
  if (parts.length < 3 || parts.length % 2 === 0) return null;
  const frag = document.createDocumentFragment();
  parts.forEach((p, i) => {
    if (i % 2 === 1) {
      const c = document.createElement("code");
      c.textContent = p;
      frag.append(c);
    } else if (p) {
      frag.append(document.createTextNode(p));
    }
  });
  return frag;
}

/// Sweep static DOM strings.
function applyStatic() {
  for (const el of document.querySelectorAll("[data-i18n]")) {
    const s = t(el.dataset.i18n);
    const frag = inlineCode(s);
    if (frag) { el.textContent = ""; el.append(frag); } else el.textContent = s;
  }
  for (const el of document.querySelectorAll("[data-i18n-ph]")) el.placeholder = t(el.dataset.i18nPh);
  for (const el of document.querySelectorAll("[data-i18n-title]")) {
    const s = t(el.dataset.i18nTitle);
    el.title = s;
    el.setAttribute("aria-label", s);
  }
}
