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
    "status.error": "Error",
    "status.launched": "Launched",
    "status.saved": "Settings saved",
    "detail.reading": "Reading the manifest…",
    "detail.installing": "Installing…",
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
    "btn.install": "Install",
    "btn.update": "Update",
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
    "set.gameHint": "The folder that contains game\\.",
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
    "wn.loading": "Loading earlier versions…",
  },
  ru: {
    "status.notChecked": "Не проверено",
    "status.checkHint": "Проверьте, чтобы прочитать манифест.",
    "status.working": "Работаю…",
    "status.upToDate": "Актуально",
    "status.updateAvail": "Есть обновление",
    "status.notInstalled": "Не установлено",
    "status.error": "Ошибка",
    "status.launched": "Запущено",
    "status.saved": "Настройки сохранены",
    "detail.reading": "Читаю манифест…",
    "detail.installing": "Устанавливаю…",
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
    "btn.install": "Установить",
    "btn.update": "Обновить",
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
    "set.gameHint": "Папка, содержащая game\\.",
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
    "wn.loading": "Загружаю предыдущие версии…",
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

/// Sweep static DOM strings.
function applyStatic() {
  for (const el of document.querySelectorAll("[data-i18n]")) el.textContent = t(el.dataset.i18n);
  for (const el of document.querySelectorAll("[data-i18n-ph]")) el.placeholder = t(el.dataset.i18nPh);
  for (const el of document.querySelectorAll("[data-i18n-title]")) {
    const s = t(el.dataset.i18nTitle);
    el.title = s;
    el.setAttribute("aria-label", s);
  }
}
