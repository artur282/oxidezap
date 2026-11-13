//! Native-shell internationalization.
//!
//! OxideZap has two supported UI languages, English and Spanish. The English
//! source text is the lookup key, so a missing Spanish entry always falls back
//! to readable English instead of an opaque identifier. This module does not
//! touch WhatsApp Web: the webview keeps whatever language WhatsApp serves.
//!
//! Adding a language means adding a table arm here and extending [`Language`].
//! The test module scans every scoped source file for the [`t!`], [`tr!`] and
//! [`tn!`] macros and fails when a key has no translation, so a forgotten
//! string cannot ship silently. See README.md, "Interface language".

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// Single-key translation macro. The key is the English source text.
macro_rules! t {
    ($key:literal $(,)?) => {
        $crate::i18n::translate($key)
    };
}

/// Translation with named placeholders:
/// `tr!("Saved to {path}", path = destination.display())`.
macro_rules! tr {
    ($key:literal $(,)?) => {
        $crate::i18n::translate($key).to_string()
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {{
        let args = [$( (stringify!($name), ($value).to_string()) ),+];
        $crate::i18n::translate_args($key, &args)
    }};
}

/// Plural helper: chooses the singular key for exactly one and interpolates
/// `{count}`. Both keys are translated and scanned for completeness.
macro_rules! tn {
    ($count:expr, $one:literal, $many:literal $(,)?) => {{
        let count = $count;
        let key = if count == 1 { $one } else { $many };
        $crate::i18n::translate_args(key, &[("count", count.to_string())])
    }};
}

/// Interface language setting. `System` follows `LC_ALL`, `LC_MESSAGES`,
/// `LANG`, or `LANGUAGE`, in that order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    System,
    English,
    Spanish,
}

impl Language {
    /// Concrete language used for rendering. `System` resolves the desktop
    /// locale and falls back to English for unsupported locales.
    pub fn resolved(configured: Language, locale: Option<&str>) -> Language {
        match configured {
            Language::English => Language::English,
            Language::Spanish => Language::Spanish,
            Language::System => locale.and_then(parse_locale).unwrap_or(Language::English),
        }
    }

    /// Human-readable name in the current interface language.
    pub fn label(self) -> &'static str {
        match self {
            Language::System => t!("System"),
            Language::English => t!("English"),
            Language::Spanish => t!("Spanish"),
        }
    }

    /// Stable identifier used by settings and tests.
    pub fn slug(self) -> &'static str {
        match self {
            Language::System => "system",
            Language::English => "english",
            Language::Spanish => "spanish",
        }
    }
}

impl Serialize for Language {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.slug())
    }
}

impl<'de> Deserialize<'de> for Language {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.trim().to_ascii_lowercase().as_str() {
            "english" | "en" => Language::English,
            "spanish" | "es" => Language::Spanish,
            _ => Language::System,
        })
    }
}

/// Reads the desktop locale from the standard environment variables, most
/// specific first. `LANGUAGE` is a colon-separated preference list.
pub fn detected_locale() -> Option<String> {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
        if let Ok(value) = std::env::var(key)
            && !value.trim().is_empty()
        {
            return Some(value);
        }
    }
    None
}

/// Parses a POSIX locale such as `es_AR.UTF-8`, `es_ES@euro`, or `en`. A
/// `LANGUAGE=fr:es` list returns the first supported entry. Unknown and
/// placeholder locales (`C`, `POSIX`) return `None`.
pub fn parse_locale(raw: &str) -> Option<Language> {
    raw.split(':').find_map(parse_single_locale)
}

fn parse_single_locale(raw: &str) -> Option<Language> {
    let base = raw
        .trim()
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .trim();
    if base.is_empty() || base.eq_ignore_ascii_case("C") || base.eq_ignore_ascii_case("POSIX") {
        return None;
    }
    let primary = base
        .split(['_', '-'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match primary.as_str() {
        "en" => Some(Language::English),
        "es" => Some(Language::Spanish),
        _ => None,
    }
}

/// The tray and notification threads read this atomic, so a language change
/// does not need to restart any background service.
static ACTIVE: AtomicU8 = AtomicU8::new(0);
const ENGLISH_ID: u8 = 0;
const SPANISH_ID: u8 = 1;

/// Resolves and stores the active interface language. Called once at startup
/// and again whenever the setting changes.
pub fn set_language(configured: Language) {
    let resolved = match configured {
        Language::System => Language::resolved(Language::System, detected_locale().as_deref()),
        explicit => explicit,
    };
    ACTIVE.store(
        if resolved == Language::Spanish {
            SPANISH_ID
        } else {
            ENGLISH_ID
        },
        Ordering::Relaxed,
    );
}

/// Concrete language currently used by the native shell.
pub fn active_language() -> Language {
    if ACTIVE.load(Ordering::Relaxed) == SPANISH_ID {
        Language::Spanish
    } else {
        Language::English
    }
}

/// Looks up a key for a concrete language. Missing Spanish entries fall back
/// to the English key; this function never panics.
pub fn translate_for(language: Language, key: &str) -> &str {
    match language {
        Language::Spanish => spanish_translation(key).unwrap_or(key),
        _ => key,
    }
}

/// Translates a key with the active interface language.
pub fn translate(key: &str) -> &str {
    translate_for(active_language(), key)
}

/// Translates an English label that is produced dynamically, such as an enum
/// label defined outside this module. The key must exist in the table; the
/// test module lists these keys explicitly.
pub fn translate_dynamic(key: &'static str) -> &'static str {
    translate_for(active_language(), key)
}

/// Translates a runtime-owned English message. Unknown messages pass through
/// unchanged so errors from lower layers never disappear.
pub fn translate_owned(key: &str) -> String {
    translate(key).to_string()
}

/// Replaces `{name}` placeholders. Unknown placeholders are left untouched so
/// a partially translated string stays visible instead of breaking.
pub fn interpolate(template: &str, args: &[(&str, String)]) -> String {
    let mut result = template.to_string();
    for (name, value) in args {
        result = result.replace(&format!("{{{name}}}"), value);
    }
    result
}

/// Translates and interpolates in one step.
pub fn translate_args(key: &str, args: &[(&str, String)]) -> String {
    interpolate(translate(key), args)
}

/// English labels produced by code outside the scoped UI files. They are
/// listed here so the completeness test can verify their translations.
#[cfg(test)]
const DYNAMIC_KEYS: &[&str] = &[
    "In progress",
    "Completed",
    "Failed",
    "Cancelled",
    "clear the cache",
    "sign out",
    "delete the account data",
    "the account no longer exists",
    "the last account cannot be deleted",
    "Enter a shortcut.",
    "Add a key to the shortcut.",
    "A global shortcut needs at least one modifier.",
    "That key is not supported for global shortcuts.",
];

#[cfg(test)]
const SPANISH_KEYS: &[(&str, &str)] = &[
    (" · partial", " · parcial"),
    (
        "'Until tomorrow' uses the quiet-hours end time as its wake hour. Changing the times requires Apply; toggles and presets apply immediately.",
        "'Hasta mañana' usa la hora de fin del horario de silencio como hora de reactivación. Cambiar las horas requiere Aplicar; los interruptores y preajustes se aplican de inmediato.",
    ),
    (
        "A WhatsApp account stays linked to your phone. OxideZap does not run a second copy of the account and cannot receive messages while the phone is offline, just like WhatsApp Web.",
        "Una cuenta de WhatsApp permanece vinculada a tu teléfono. OxideZap no ejecuta una segunda copia de la cuenta y no puede recibir mensajes mientras el teléfono está desconectado, igual que WhatsApp Web.",
    ),
    (
        "A global palette shortcut takes precedence over WhatsApp Web's own Ctrl+K chat search.",
        "Un atajo global de la paleta tiene prioridad sobre la búsqueda de chats Ctrl+K de WhatsApp Web.",
    ),
    (
        "A native Linux shell around the official WhatsApp Web experience.",
        "Una carcasa nativa de Linux alrededor de la experiencia oficial de WhatsApp Web.",
    ),
    (
        "A newer release is published on GitHub. OxideZap does not download or install it automatically.",
        "Hay una versión más reciente publicada en GitHub. OxideZap no la descarga ni la instala automáticamente.",
    ),
    (
        "A second global shortcut for instant privacy. A second press restores the windows that were visible.",
        "Un segundo atajo global para privacidad instantánea. Al pulsarlo de nuevo se restauran las ventanas que estaban visibles.",
    ),
    ("About", "Acerca de"),
    ("About OxideZap", "Acerca de OxideZap"),
    ("Accessibility", "Accesibilidad"),
    ("Identity", "Identidad"),
    (
        "Use a hex color such as #1d4ed8.",
        "Usa un color hexadecimal como #1d4ed8.",
    ),
    (
        "Use up to two characters or one emoji. Empty shows the account initial.",
        "Usa hasta dos caracteres o un emoji. Vacío muestra la inicial de la cuenta.",
    ),
    (
        "Use up to two characters or one emoji.",
        "Usa hasta dos caracteres o un emoji.",
    ),
    ("Account", "Cuenta"),
    ("Account appearance", "Apariencia de la cuenta"),
    ("Account data", "Datos de la cuenta"),
    ("Account isolation", "Aislamiento de cuentas"),
    ("Account name", "Nombre de la cuenta"),
    ("Account suspended", "Cuenta suspendida"),
    ("Accounts", "Cuentas"),
    ("Accounts and data", "Cuentas y datos"),
    (
        "Accounts from the file replace the current account list; local session data is not deleted. The app-lock password is not imported, so set it again afterwards.",
        "Las cuentas del archivo reemplazan la lista actual; los datos de sesión locales no se eliminan. La contraseña del bloqueo no se importa, así que vuelve a configurarla después.",
    ),
    ("Accounts: {from} -> {to}", "Cuentas: {from} -> {to}"),
    ("Actions", "Acciones"),
    ("Active views", "Vistas activas"),
    (
        "Add CSS before enabling experimental styles.",
        "Añade CSS antes de activar los estilos experimentales.",
    ),
    (
        "Add a YYYY-MM subfolder (UTC) for the month each file was downloaded.",
        "Añade una subcarpeta AAAA-MM (UTC) para el mes en que se descargó cada archivo.",
    ),
    ("Add account", "Añadir cuenta"),
    ("Add account…", "Añadir cuenta…"),
    (
        "Adding an account opens a new window with a fresh QR code.",
        "Añadir una cuenta abre una ventana nueva con un código QR nuevo.",
    ),
    ("Always connect directly", "Conectar siempre directamente"),
    (
        "Another storage operation is already running.",
        "Ya hay otra operación de almacenamiento en curso.",
    ),
    ("App", "Aplicación"),
    ("App lock", "Bloqueo de la aplicación"),
    ("Appearance", "Apariencia"),
    (
        "Appearance applied to all accounts.",
        "Apariencia aplicada a todas las cuentas.",
    ),
    ("Apply boss key", "Aplicar atajo de jefe"),
    ("Apply experimental CSS", "Aplicar CSS experimental"),
    ("Apply folder", "Aplicar carpeta"),
    ("Apply import", "Aplicar importación"),
    ("Apply palette shortcut", "Aplicar atajo de la paleta"),
    ("Apply proxy settings", "Aplicar ajustes del proxy"),
    ("Apply quiet hours", "Aplicar horario de silencio"),
    ("Apply shortcut", "Aplicar atajo"),
    (
        "Apply this account's appearance to all accounts?",
        "¿Aplicar la apariencia de esta cuenta a todas las cuentas?",
    ),
    ("Apply to all accounts", "Aplicar a todas las cuentas"),
    (
        "Ask GitHub for the latest release at most once every 24 hours. The request contains no personal data.",
        "Consulta a GitHub la última versión como máximo una vez cada 24 horas. La solicitud no contiene datos personales.",
    ),
    (
        "Ask for the password when OxideZap starts.",
        "Pide la contraseña cuando OxideZap se inicia.",
    ),
    (
        "Ask the desktop notification service to play a message sound.",
        "Pide al servicio de notificaciones del escritorio que reproduzca un sonido de mensaje.",
    ),
    (
        "At least {count} characters.",
        "Al menos {count} caracteres.",
    ),
    (
        "Use a stronger palette for text, backgrounds, and borders in the native shell. The System, Light, and Dark choice still picks the base palette; WhatsApp Web content is unchanged.",
        "Usa una paleta con más contraste para el texto, los fondos y los bordes de la carcasa nativa. La opción Sistema, Claro u Oscuro sigue eligiendo la paleta base; el contenido de WhatsApp Web no cambia.",
    ),
    (
        "Use at least {count} characters.",
        "Usa al menos {count} caracteres.",
    ),
    ("Automatic", "Automático"),
    (
        "Automatic follows the desktop locale. WebKitGTK only checks languages whose hunspell dictionaries are installed. Changes apply to open accounts; WhatsApp Web still controls its own composer and Enter-to-send behavior.",
        "Automático sigue la configuración regional del escritorio. WebKitGTK solo revisa los idiomas cuyos diccionarios hunspell están instalados. Los cambios se aplican a las cuentas abiertas; WhatsApp Web sigue controlando su propio editor y el comportamiento de Enter para enviar.",
    ),
    (
        "Automatic recovery failed after {attempts} attempts. Use “Retry now” in the account window.",
        "La recuperación automática falló tras {attempts} intentos. Usa «Reintentar ahora» en la ventana de la cuenta.",
    ),
    (
        "Automatic update checks are off.",
        "Las comprobaciones automáticas de actualizaciones están desactivadas.",
    ),
    ("Avatar", "Avatar"),
    ("Back", "Atrás"),
    ("Backup and restore", "Copia de seguridad y restauración"),
    (
        "Best effort: depends on the desktop exposing a ScreenSaver D-Bus interface.",
        "Mejor esfuerzo: depende de que el escritorio exponga una interfaz D-Bus de ScreenSaver.",
    ),
    ("Boss key", "Atajo de jefe"),
    ("Boss key: {from} -> {to}", "Atajo de jefe: {from} -> {to}"),
    ("Bright surfaces", "Superficies claras"),
    ("Built with", "Creado con"),
    ("CSS files (*.css)", "Archivos CSS (*.css)"),
    (
        "CSS loaded and applied to open accounts.",
        "CSS cargado y aplicado a las cuentas abiertas.",
    ),
    (
        "CSS loaded. Use the toggle above to apply it.",
        "CSS cargado. Usa el interruptor de arriba para aplicarlo.",
    ),
    (
        "Cache cleared ({freed} freed). The WhatsApp session was kept.",
        "Caché borrada ({freed} liberados). Se conservó la sesión de WhatsApp.",
    ),
    ("Cancel", "Cancelar"),
    ("Cancelled", "Cancelada"),
    ("Change password", "Cambiar contraseña"),
    (
        "Check for updates automatically",
        "Buscar actualizaciones automáticamente",
    ),
    (
        "Choose a local .css file to load a stylesheet.",
        "Elige un archivo .css local para cargar una hoja de estilos.",
    ),
    (
        "Choose how WhatsApp Web reaches the network. Changes apply per account and may require reopening an account.",
        "Elige cómo accede WhatsApp Web a la red. Los cambios se aplican por cuenta y pueden requerir reabrir una cuenta.",
    ),
    (
        "Choose how the native shell looks and how large WhatsApp Web is rendered.",
        "Elige cómo se ve la carcasa nativa y con qué tamaño se renderiza WhatsApp Web.",
    ),
    (
        "Choose the OxideZap download folder",
        "Elige la carpeta de descargas de OxideZap",
    ),
    ("Choose…", "Elegir…"),
    ("Clear cache", "Borrar caché"),
    (
        "Clearing data closes and recreates the account's webview. Clearing the cache keeps the WhatsApp session; signing out requires the QR code again. Sizes are approximate while WhatsApp Web is running and stop at a bounded scan limit.",
        "Borrar datos cierra y recrea la vista web de la cuenta. Borrar la caché conserva la sesión de WhatsApp; cerrar sesión requiere el código QR de nuevo. Los tamaños son aproximados mientras WhatsApp Web está en ejecución y se detienen en un límite de escaneo acotado.",
    ),
    ("Close", "Cerrar"),
    (
        "Close the IPv6 address with a bracket, for example [::1].",
        "Cierra la dirección IPv6 con un corchete, por ejemplo [::1].",
    ),
    ("Close to tray", "Cerrar a la bandeja"),
    ("Close to tray: {state}", "Cerrar a la bandeja: {state}"),
    (
        "Closing an account window hides it instead of quitting OxideZap.",
        "Cerrar una ventana de cuenta la oculta en lugar de salir de OxideZap.",
    ),
    (
        "Closing an account window hides it instead of quitting. Reopen it from the tray icon or the global show/hide shortcut; turn this off in Settings > General.",
        "Cerrar una ventana de cuenta la oculta en lugar de salir. Vuelve a abrirla desde el icono de la bandeja o el atajo global de mostrar/ocultar; desactívalo en Ajustes > General.",
    ),
    ("Command palette", "Paleta de comandos"),
    ("Command palette (Ctrl+K)", "Paleta de comandos (Ctrl+K)"),
    (
        "Command palette shortcut: {from} -> {to}",
        "Atajo de la paleta de comandos: {from} -> {to}",
    ),
    ("Complete", "Completo"),
    ("Completed", "Completada"),
    (
        "Completed (guide version {version}). Reopening it here never marks it uncompleted.",
        "Completada (versión de la guía {version}). Volver a abrirla aquí nunca la marca como incompleta.",
    ),
    ("Config", "Configuración"),
    ("Confirm password", "Confirmar contraseña"),
    ("Connected", "Conectado"),
    (
        "Connected directly to web.whatsapp.com:443. No proxy is configured.",
        "Conexión directa a web.whatsapp.com:443. No hay ningún proxy configurado.",
    ),
    (
        "Connected directly to web.whatsapp.com:443. The desktop has no system proxy configured.",
        "Conexión directa a web.whatsapp.com:443. El escritorio no tiene un proxy del sistema configurado.",
    ),
    ("Connecting…", "Conectando…"),
    ("Connection", "Conexión"),
    ("Connection mode", "Modo de conexión"),
    ("Connection notifications", "Notificaciones de conexión"),
    (
        "Connection notifications also follow Do not disturb and per-account mute.",
        "Las notificaciones de conexión también respetan No molestar y el silencio por cuenta.",
    ),
    (
        "Connection notifications: {state}",
        "Notificaciones de conexión: {state}",
    ),
    ("Connection restored", "Conexión restaurada"),
    (
        "Contrast, text size, and motion preferences for the native shell. They are saved with your settings and included in exports.",
        "Preferencias de contraste, tamaño del texto y movimiento para la carcasa nativa. Se guardan con tu configuración y se incluyen en las exportaciones.",
    ),
    ("Copy", "Copiar"),
    ("Correct while I type", "Corregir mientras escribo"),
    (
        "Could not authenticate with the SOCKS proxy: {error}",
        "No se pudo autenticar con el proxy SOCKS: {error}",
    ),
    (
        "Could not back up the current settings, so nothing was imported: {error}",
        "No se pudo hacer una copia de la configuración actual, así que no se importó nada: {error}",
    ),
    (
        "Could not back up the current settings, so nothing was reset: {error}",
        "No se pudo hacer una copia de la configuración actual, así que no se restableció nada: {error}",
    ),
    (
        "Could not export settings: {error}",
        "No se pudo exportar la configuración: {error}",
    ),
    (
        "Could not import settings: {error}",
        "No se pudo importar la configuración: {error}",
    ),
    (
        "Could not inspect {path}: {error}",
        "No se pudo inspeccionar {path}: {error}",
    ),
    (
        "Could not open the account window",
        "No se pudo abrir la ventana de la cuenta",
    ),
    (
        "Could not reach {label} at {host}:{port}: {error}",
        "No se pudo alcanzar {label} en {host}:{port}: {error}",
    ),
    (
        "Could not read the SOCKS address: {error}",
        "No se pudo leer la dirección SOCKS: {error}",
    ),
    (
        "Could not read the SOCKS reply: {error}",
        "No se pudo leer la respuesta SOCKS: {error}",
    ),
    (
        "Could not read the desktop proxy settings: {error}",
        "No se pudo leer la configuración del proxy del escritorio: {error}",
    ),
    (
        "Could not read {path}: {error}",
        "No se pudo leer {path}: {error}",
    ),
    (
        "Could not reconnect WhatsApp Web",
        "No se pudo reconectar WhatsApp Web",
    ),
    (
        "Could not recover WhatsApp Web",
        "No se pudo recuperar WhatsApp Web",
    ),
    (
        "Could not resolve {label} ({host}): {error}",
        "No se pudo resolver {label} ({host}): {error}",
    ),
    (
        "Could not send the CONNECT request to the proxy: {error}",
        "No se pudo enviar la solicitud CONNECT al proxy: {error}",
    ),
    (
        "Could not send the SOCKS CONNECT request: {error}",
        "No se pudo enviar la solicitud CONNECT de SOCKS: {error}",
    ),
    (
        "Could not start the SOCKS negotiation: {error}",
        "No se pudo iniciar la negociación SOCKS: {error}",
    ),
    (
        "Could not write {path}: {error}",
        "No se pudo escribir {path}: {error}",
    ),
    (
        "Could not {operation}: {error}",
        "No se pudo {operation}: {error}",
    ),
    (
        "Creating a fresh WebKit view. Your session is kept and the page will reconnect automatically.",
        "Creando una vista de WebKit nueva. Tu sesión se conserva y la página se reconectará automáticamente.",
    ),
    ("Custom color", "Color personalizado"),
    ("Dark", "Oscuro"),
    ("Data folder: {path}", "Carpeta de datos: {path}"),
    (
        "Data {data} · Cache {cache} · Total {total}{partial}",
        "Datos {data} · Caché {cache} · Total {total}{partial}",
    ),
    ("Delete account data", "Eliminar datos de la cuenta"),
    (
        "Delete all data for “{name}”?",
        "¿Eliminar todos los datos de «{name}»?",
    ),
    (
        "Deleted the account data and removed the account ({freed} freed).",
        "Se eliminaron los datos y la cuenta ({freed} liberados).",
    ),
    (
        "Deleting account data requires typing the account name and removes the account from OxideZap. The last account cannot be deleted.",
        "Eliminar los datos de una cuenta requiere escribir su nombre y la quita de OxideZap. La última cuenta no se puede eliminar.",
    ),
    ("Desktop notifications", "Notificaciones de escritorio"),
    (
        "Desktop notifications: {state}",
        "Notificaciones de escritorio: {state}",
    ),
    (
        "Destroy the WebKit view of an account that stayed hidden for the delay below. The account and its WhatsApp session stay on disk.",
        "Destruye la vista de WebKit de una cuenta que permaneció oculta durante el tiempo indicado abajo. La cuenta y su sesión de WhatsApp permanecen en disco.",
    ),
    ("Development build", "Compilación de desarrollo"),
    (
        "Development builds do not check for updates.",
        "Las compilaciones de desarrollo no buscan actualizaciones.",
    ),
    ("Disabled", "Desactivado"),
    ("Display", "Pantalla"),
    ("Display size", "Tamaño de pantalla"),
    ("Do not disturb", "No molestar"),
    ("Do not disturb: {state}", "No molestar: {state}"),
    ("Do not disturb: {status}", "No molestar: {status}"),
    ("Download cancelled", "Descarga cancelada"),
    ("Download complete", "Descarga completada"),
    (
        "Download could not be retried",
        "No se pudo reintentar la descarga",
    ),
    ("Download could not start", "No se pudo iniciar la descarga"),
    ("Download failed", "Descarga fallida"),
    ("Download folder", "Carpeta de descargas"),
    ("Download in progress", "Descarga en curso"),
    ("Downloading", "Descargando"),
    ("Downloads", "Descargas"),
    (
        "Downloads appear here as they start.",
        "Las descargas aparecen aquí cuando comienzan.",
    ),
    (
        "Downloads are written straight to disk. In-progress rows can be cancelled; finished rows can be retried when WebKit exposed a source URL.",
        "Las descargas se escriben directamente en disco. Las filas en curso se pueden cancelar; las terminadas se pueden reintentar cuando WebKit expuso una URL de origen.",
    ),
    ("Downloads…", "Descargas…"),
    (
        "Enable the loaded stylesheet on every open WhatsApp Web account.",
        "Activa la hoja de estilos cargada en cada cuenta de WhatsApp Web abierta.",
    ),
    ("End", "Fin"),
    ("English", "Inglés"),
    (
        "Enter a proxy username when a password is set.",
        "Introduce un usuario del proxy cuando hay una contraseña configurada.",
    ),
    (
        "Enter a valid IPv6 address in brackets, for example [::1].",
        "Introduce una dirección IPv6 válida entre corchetes, por ejemplo [::1].",
    ),
    (
        "Enter an HTTP, HTTPS, or SOCKS proxy",
        "Introduce un proxy HTTP, HTTPS o SOCKS",
    ),
    (
        "Enter an absolute folder path, or use the default.",
        "Introduce una ruta de carpeta absoluta o usa la predeterminada.",
    ),
    (
        "Enter both quiet-hours times as HH:MM.",
        "Introduce ambas horas del silencio como HH:MM.",
    ),
    ("Enter the proxy host.", "Introduce el host del proxy."),
    ("Enter the proxy port.", "Introduce el puerto del proxy."),
    (
        "Enter your password to return to WhatsApp. Account windows stay hidden until you unlock.",
        "Introduce tu contraseña para volver a WhatsApp. Las ventanas de cuenta permanecen ocultas hasta que desbloquees.",
    ),
    ("Enter your password.", "Introduce tu contraseña."),
    ("Estimated memory released", "Memoria liberada estimada"),
    (
        "Every account gets its own window and its own WebKit profile, so two WhatsApp accounts never share cookies, local storage, or caches.",
        "Cada cuenta tiene su propia ventana y su propio perfil de WebKit, así que dos cuentas de WhatsApp nunca comparten cookies, almacenamiento local ni cachés.",
    ),
    (
        "Every account has its own isolated WhatsApp Web session and window.",
        "Cada cuenta tiene su propia sesión de WhatsApp Web aislada y su propia ventana.",
    ),
    ("Every day", "Todos los días"),
    ("Experimental CSS", "CSS experimental"),
    (
        "Experimental CSS disabled.",
        "CSS experimental desactivado.",
    ),
    ("Experimental CSS enabled.", "CSS experimental activado."),
    (
        "Experimental CSS removed. WhatsApp Web uses its own styles again.",
        "CSS experimental eliminado. WhatsApp Web vuelve a usar sus propios estilos.",
    ),
    (
        "Experimental CSS: {state} (custom CSS is never imported)",
        "CSS experimental: {state} (el CSS personalizado nunca se importa)",
    ),
    ("Export", "Exportar"),
    (
        "Export OxideZap settings",
        "Exportar configuración de OxideZap",
    ),
    ("Export settings…", "Exportar configuración…"),
    (
        "Export the configuration to a readable JSON file, import it on another installation, or restore the defaults.",
        "Exporta la configuración a un archivo JSON legible, impórtala en otra instalación o restaura los valores predeterminados.",
    ),
    (
        "Exports never include account sessions, cookies, chats, or the app-lock password. Every import and reset writes a dated backup first.",
        "Las exportaciones nunca incluyen sesiones de cuenta, cookies, chats ni la contraseña del bloqueo. Cada importación y restablecimiento escribe antes una copia de seguridad con fecha.",
    ),
    ("FIRST-RUN GUIDE", "GUÍA INICIAL"),
    ("Failed", "Fallida"),
    ("File", "Archivo"),
    (
        "Files saved from WhatsApp Web show their progress and history here. Blob and view-once media never reach this list.",
        "Los archivos guardados desde WhatsApp Web muestran aquí su progreso e historial. Los medios blob y de ver una vez nunca llegan a esta lista.",
    ),
    ("First-run guide", "Guía inicial"),
    ("Folder", "Carpeta"),
    ("Follow quiet hours", "Seguir el horario de silencio"),
    ("Follow the desktop", "Seguir el escritorio"),
    ("Follow the system theme", "Seguir el tema del sistema"),
    ("For 1 hour", "Durante 1 hora"),
    ("Free disk space", "Espacio libre en disco"),
    (
        "GPUI 0.2.2 has no AccessKit/AT-SPI bridge, so screen readers cannot inspect these native controls. Keyboard focus and the visible focus ring still work, and WhatsApp Web keeps WebKit's own accessibility tree.",
        "GPUI 0.2.2 no tiene un puente AccessKit/AT-SPI, así que los lectores de pantalla no pueden inspeccionar estos controles nativos. El foco de teclado y el anillo de foco visible siguen funcionando, y WhatsApp Web conserva el árbol de accesibilidad propio de WebKit.",
    ),
    (
        "GPUI for the native shell · WebKitGTK for WhatsApp Web · Rust for everything in between.",
        "GPUI para la carcasa nativa · WebKitGTK para WhatsApp Web · Rust para todo lo demás.",
    ),
    ("General", "General"),
    ("GitHub repository", "Repositorio de GitHub"),
    (
        "Global appearance: {appearance}",
        "Apariencia global: {appearance}",
    ),
    ("Global shortcut", "Atajo global"),
    (
        "Global shortcut: {from} -> {to}",
        "Atajo global: {from} -> {to}",
    ),
    ("Global shortcuts", "Atajos globales"),
    (
        "Global shortcuts require an X11 or XWayland session.",
        "Los atajos globales requieren una sesión X11 o XWayland.",
    ),
    ("HH:MM, local time", "HH:MM, hora local"),
    (
        "HH:MM, may cross midnight",
        "HH:MM, puede cruzar la medianoche",
    ),
    ("Hide OxideZap", "Ocultar OxideZap"),
    (
        "Hide all account windows",
        "Ocultar todas las ventanas de cuenta",
    ),
    (
        "Hide every account window behind a lock screen.",
        "Oculta todas las ventanas de cuenta tras una pantalla de bloqueo.",
    ),
    (
        "Hide or restore every window",
        "Ocultar o restaurar todas las ventanas",
    ),
    ("High contrast", "Alto contraste"),
    ("High contrast: {state}", "Alto contraste: {state}"),
    ("Host", "Host"),
    ("Image copied", "Imagen copiada"),
    ("Import", "Importar"),
    (
        "Import OxideZap settings",
        "Importar configuración de OxideZap",
    ),
    ("Import settings…", "Importar configuración…"),
    ("Import summary", "Resumen de la importación"),
    ("In progress", "En curso"),
    ("Incorrect password.", "Contraseña incorrecta."),
    ("Indefinitely", "Indefinidamente"),
    (
        "Inspect and clean the WebKit profile behind each account.",
        "Inspecciona y limpia el perfil de WebKit de cada cuenta.",
    ),
    ("Interface language", "Idioma de la interfaz"),
    (
        "Interface language: {from} -> {to}",
        "Idioma de la interfaz: {from} -> {to}",
    ),
    (
        "It can take a few seconds. If the page stays blank, check that WebKitGTK 2.46.1 or newer is installed; older versions can hang the QR login. OxideZap never waits for the QR before showing this guide.",
        "Puede tardar unos segundos. Si la página sigue en blanco, comprueba que WebKitGTK 2.46.1 o posterior esté instalado; las versiones anteriores pueden colgar el inicio de sesión por QR. OxideZap nunca espera al QR antes de mostrar esta guía.",
    ),
    ("Just now", "Ahora mismo"),
    (
        "Keep the active account loaded",
        "Mantener cargada la cuenta activa",
    ),
    ("Last cleanup", "Última limpieza"),
    ("Launch at login", "Iniciar al iniciar sesión"),
    (
        "Launch at login: {state}",
        "Iniciar al iniciar sesión: {state}",
    ),
    (
        "Leave the default to follow the desktop Downloads directory.",
        "Deja el valor predeterminado para seguir la carpeta de Descargas del escritorio.",
    ),
    ("Light", "Claro"),
    ("Link your phone", "Vincula tu teléfono"),
    ("Load", "Cargar"),
    ("Load CSS file…", "Cargar archivo CSS…"),
    (
        "Load experimental WhatsApp CSS",
        "Cargar CSS experimental de WhatsApp",
    ),
    ("Loading WhatsApp Web…", "Cargando WhatsApp Web…"),
    ("Lock", "Bloquear"),
    ("Lock OxideZap", "Bloquear OxideZap"),
    ("Lock after idle time", "Bloquear tras inactividad"),
    (
        "Lock automatically when the window is hidden to the tray.",
        "Bloquea automáticamente cuando la ventana se oculta en la bandeja.",
    ),
    (
        "Lock before sleeping and after waking, using the logind D-Bus signal when available.",
        "Bloquea antes de suspender y al despertar, usando la señal D-Bus de logind cuando esté disponible.",
    ),
    ("Lock now", "Bloquear ahora"),
    ("Lock on launch", "Bloquear al iniciar"),
    ("Lock on system suspend", "Bloquear al suspender el sistema"),
    ("Lock when hidden", "Bloquear al ocultar"),
    (
        "Lock when the desktop screen locks",
        "Bloquear cuando se bloquee la pantalla",
    ),
    (
        "Log in to a second number from Settings > Accounts > Add account. Removing an account also removes its local profile.",
        "Inicia sesión con un segundo número desde Ajustes > Cuentas > Añadir cuenta. Quitar una cuenta también elimina su perfil local.",
    ),
    ("Low-light surfaces", "Superficies oscuras"),
    ("Low-memory mode", "Modo de bajo consumo de memoria"),
    (
        "Low-memory mode: {state}",
        "Modo de bajo consumo de memoria: {state}",
    ),
    (
        "MIT licensed. The source code is available on GitHub.",
        "Licencia MIT. El código fuente está disponible en GitHub.",
    ),
    ("Manual proxy", "Proxy manual"),
    ("Maximize", "Maximizar"),
    ("Media action failed", "La acción de medios falló"),
    ("Media action menu", "Menú de acciones de medios"),
    (
        "Media action menu: {state}",
        "Menú de acciones de medios: {state}",
    ),
    (
        "Message previews use your desktop's notification service. Toggle them globally, mute one account, or turn on Do Not Disturb in Settings > Notifications.",
        "Las vistas previas de mensajes usan el servicio de notificaciones del escritorio. Actívalas globalmente, silencia una cuenta o activa No molestar en Ajustes > Notificaciones.",
    ),
    ("Minimize", "Minimizar"),
    (
        "Mode: {mode} · Status: {status}",
        "Modo: {mode} · Estado: {status}",
    ),
    ("Mute notifications", "Silenciar notificaciones"),
    ("Muted", "Silenciada"),
    (
        "Muted accounts and Do not disturb have priority over per-account sounds.",
        "Las cuentas silenciadas y No molestar tienen prioridad sobre los sonidos por cuenta.",
    ),
    ("Native notifications", "Notificaciones nativas"),
    ("Network", "Red"),
    (
        "Network proxy credentials: removed",
        "Credenciales del proxy de red: eliminadas",
    ),
    (
        "Network proxy: {from} -> {to}",
        "Proxy de red: {from} -> {to}",
    ),
    ("Never", "Nunca"),
    (
        "Never release the account you opened most recently, even when every window is hidden.",
        "Nunca liberes la cuenta que abriste más recientemente, incluso cuando todas las ventanas estén ocultas.",
    ),
    ("New password", "Contraseña nueva"),
    ("Next", "Siguiente"),
    ("No downloads yet", "Aún no hay descargas"),
    (
        "No hidden account could be released right now. Downloads, calls or recordings, the active account, the app lock, and quitting all block suspension.",
        "No se pudo liberar ninguna cuenta oculta ahora mismo. Las descargas, llamadas o grabaciones, la cuenta activa, el bloqueo de la aplicación y el cierre bloquean la suspensión.",
    ),
    ("No matching commands", "No hay comandos coincidentes"),
    ("No preference changes.", "No hay cambios de preferencias."),
    ("No proxy", "Sin proxy"),
    (
        "No stylesheet loaded.",
        "No hay ninguna hoja de estilos cargada.",
    ),
    ("No telemetry", "Sin telemetría"),
    ("No unread messages", "No hay mensajes sin leer"),
    (
        "Not completed. It opens automatically only on a new installation.",
        "No completada. Solo se abre automáticamente en una instalación nueva.",
    ),
    ("Not scanned yet", "Aún no escaneado"),
    ("Notification sound", "Sonido de notificación"),
    ("Notifications", "Notificaciones"),
    ("Notifications and sound", "Notificaciones y sonido"),
    ("Notifications, silent", "Notificaciones, silenciosas"),
    ("Off", "Desactivado"),
    ("Offline", "Desconectado"),
    ("On", "Activado"),
    ("On for 1 hour", "Durante 1 hora"),
    (
        "On until you turn it off",
        "Activado hasta que lo desactives",
    ),
    (
        "On until {point} ({remaining})",
        "Activado hasta {point} ({remaining})",
    ),
    ("Open", "Abrir"),
    ("Open Download Center", "Abrir el centro de descargas"),
    ("Open WhatsApp Web", "Abrir WhatsApp Web"),
    (
        "Open WhatsApp on your phone.",
        "Abre WhatsApp en tu teléfono.",
    ),
    ("Open data folder", "Abrir carpeta de datos"),
    ("Open download center", "Abrir el centro de descargas"),
    ("Open download folder", "Abrir carpeta de descargas"),
    ("Open downloads folder", "Abrir carpeta de descargas"),
    ("Open folder", "Abrir carpeta"),
    ("Open settings", "Abrir ajustes"),
    ("Open storage folder", "Abrir carpeta de almacenamiento"),
    (
        "Open the Download Center to retry.",
        "Abre el centro de descargas para reintentar.",
    ),
    (
        "Opens the command palette while any application is focused. Leave empty to use Ctrl+K only inside the native window.",
        "Abre la paleta de comandos mientras cualquier aplicación tiene el foco. Déjalo vacío para usar Ctrl+K solo dentro de la ventana nativa.",
    ),
    ("Optional", "Opcional"),
    ("Optional global shortcut", "Atajo global opcional"),
    ("Optional password", "Contraseña opcional"),
    ("Optional username", "Usuario opcional"),
    (
        "Optional, e.g. Ctrl+Shift+K",
        "Opcional, p. ej. Ctrl+Shift+K",
    ),
    (
        "Optional, stored as plain text",
        "Opcional, guardada como texto plano",
    ),
    ("Organize by account", "Organizar por cuenta"),
    (
        "Other accounts will use the same theme and zoom overrides. Their sessions and window bounds are kept.",
        "Las demás cuentas usarán el mismo tema y los mismos ajustes de zoom. Sus sesiones y límites de ventana se conservan.",
    ),
    ("Organize by month", "Organizar por mes"),
    ("OxideZap Downloads", "Descargas de OxideZap"),
    ("OxideZap Locked", "OxideZap bloqueado"),
    ("OxideZap Settings", "Ajustes de OxideZap"),
    ("OxideZap command palette", "Paleta de comandos de OxideZap"),
    (
        "OxideZap creates its files with private 0600/0700 permissions. The download folder stays shared with your desktop.",
        "OxideZap crea sus archivos con permisos privados 0600/0700. La carpeta de descargas sigue compartida con tu escritorio.",
    ),
    (
        "OxideZap did not reload WhatsApp Web while a download is active.",
        "OxideZap no recargó WhatsApp Web mientras hay una descarga activa.",
    ),
    (
        "OxideZap did not reload WhatsApp Web while a download is active. Retry when the download finishes.",
        "OxideZap no recargó WhatsApp Web mientras hay una descarga activa. Reinténtalo cuando termine la descarga.",
    ),
    (
        "OxideZap is an unofficial, independent project. It is not affiliated with, endorsed by, or sponsored by WhatsApp or Meta. WhatsApp is a trademark of Meta Platforms, Inc.",
        "OxideZap es un proyecto independiente y no oficial. No está afiliado, respaldado ni patrocinado por WhatsApp o Meta. WhatsApp es una marca de Meta Platforms, Inc.",
    ),
    ("OxideZap is locked", "OxideZap está bloqueado"),
    (
        "OxideZap is trying to recover the page.",
        "OxideZap está intentando recuperar la página.",
    ),
    (
        "OxideZap settings (*.json)",
        "Configuración de OxideZap (*.json)",
    ),
    (
        "OxideZap stopped automatic reconnects for {name} after {attempts} attempts. Use Retry now in the account window.",
        "OxideZap detuvo las reconexiones automáticas de {name} tras {attempts} intentos. Usa Reintentar ahora en la ventana de la cuenta.",
    ),
    (
        "OxideZap will reconnect when the network returns.",
        "OxideZap se reconectará cuando vuelva la red.",
    ),
    (
        "OxideZap wraps the official WhatsApp Web in a native Linux window. It is an independent, unofficial project, not affiliated with or endorsed by WhatsApp or Meta.",
        "OxideZap envuelve WhatsApp Web oficial en una ventana nativa de Linux. Es un proyecto independiente y no oficial, no afiliado ni respaldado por WhatsApp o Meta.",
    ),
    (
        "OxideZap — {count} unread message",
        "OxideZap — {count} mensaje sin leer",
    ),
    (
        "OxideZap — {count} unread messages",
        "OxideZap — {count} mensajes sin leer",
    ),
    ("Password", "Contraseña"),
    (
        "Pause CSS animations and transitions in WhatsApp Web. The native shell has no animated transitions today; any future native transition must honor this setting too. It works independently of battery saver.",
        "Pausa las animaciones y transiciones CSS de WhatsApp Web. La carcasa nativa no tiene transiciones animadas hoy; cualquier transición nativa futura también deberá respetar esta opción. Funciona independientemente del ahorro de batería.",
    ),
    (
        "Pause native notifications and sounds from every account. Unread counts, window titles, and the tray badge keep updating.",
        "Pausa las notificaciones nativas y los sonidos de todas las cuentas. Los contadores sin leer, los títulos de ventana y la insignia de la bandeja siguen actualizándose.",
    ),
    ("Per-account", "Por cuenta"),
    ("Performance", "Rendimiento"),
    (
        "Performance mode: {from} -> {to}",
        "Modo de rendimiento: {from} -> {to}",
    ),
    (
        "Point the phone at the QR code in the OxideZap window.",
        "Apunta el teléfono al código QR de la ventana de OxideZap.",
    ),
    ("Port", "Puerto"),
    (
        "Prefer the full walkthrough? The README documents the same steps.",
        "¿Prefieres el recorrido completo? El README documenta los mismos pasos.",
    ),
    (
        "Preferences reset to their defaults. Your accounts were kept and a backup was written to {path}.",
        "Preferencias restablecidas a sus valores predeterminados. Tus cuentas se conservaron y se escribió una copia de seguridad en {path}.",
    ),
    ("Preview text", "Texto de vista previa"),
    ("Progress", "Progreso"),
    (
        "Proxy credentials are stored as plain text in settings.json. The file is private (0600) and credentials are never logged or written to exports, but this build does not use a system keyring. Prefer a proxy that does not require a password.",
        "Las credenciales del proxy se guardan como texto plano en settings.json. El archivo es privado (0600) y las credenciales nunca se registran ni se escriben en exportaciones, pero esta compilación no usa un llavero del sistema. Prefiere un proxy que no requiera contraseña.",
    ),
    ("Proxy protocol", "Protocolo del proxy"),
    (
        "Proxy settings saved. Reopen an account if the new route does not take effect.",
        "Ajustes del proxy guardados. Reabre una cuenta si la nueva ruta no surte efecto.",
    ),
    ("QR code does not load?", "¿No se carga el código QR?"),
    ("Quiet hours", "Horario de silencio"),
    ("Quiet hours from {point}", "Silencio desde {point}"),
    (
        "Quiet hours have no active days",
        "El silencio no tiene días activos",
    ),
    ("Quiet hours on", "Silencio activado"),
    (
        "Quiet hours skipped until {point}",
        "Silencio omitido hasta {point}",
    ),
    (
        "Quiet hours until {point} ({remaining})",
        "Silencio hasta {point} ({remaining})",
    ),
    (
        "Quiet hours use the desktop's local time. Overnight ranges such as 22:00-07:00 are supported; each period belongs to the day it starts. Times are re-evaluated when the clock changes or the machine resumes, and daylight-saving transitions follow the desktop timezone.",
        "El horario de silencio usa la hora local del escritorio. Se admiten rangos nocturnos como 22:00-07:00; cada periodo pertenece al día en que comienza. Las horas se reevalúan cuando cambia el reloj o el equipo se reanuda, y los cambios de horario de verano siguen la zona horaria del escritorio.",
    ),
    ("Quit", "Salir"),
    ("Quit OxideZap", "Salir de OxideZap"),
    ("Reactivate", "Reactivar"),
    (
        "Reactivating reloads WhatsApp Web and can take a few seconds. The current scroll position or an unsent draft may be lost.",
        "Reactivar recarga WhatsApp Web y puede tardar unos segundos. La posición de desplazamiento actual o un borrador sin enviar podrían perderse.",
    ),
    (
        "Reactivating reloads WhatsApp Web, which takes a few seconds. The current scroll position or an unsent draft may be lost.",
        "Reactivar recarga WhatsApp Web, lo que tarda unos segundos. La posición de desplazamiento actual o un borrador sin enviar podrían perderse.",
    ),
    ("Read the guide", "Leer la guía"),
    ("Reconnecting…", "Reconectando…"),
    (
        "Recurring window: {window}.",
        "Ventana recurrente: {window}.",
    ),
    (
        "Reduce animation and polling work without turning off notifications, downloads, calls, the app lock, or security checks.",
        "Reduce el trabajo de animación y sondeo sin desactivar notificaciones, descargas, llamadas, el bloqueo de la aplicación ni las comprobaciones de seguridad.",
    ),
    ("Reduce motion", "Reducir movimiento"),
    (
        "Reduce motion is independent of battery saver and stays active when the saver is off or the device is on AC power.",
        "Reducir movimiento es independiente del ahorro de batería y sigue activo cuando el ahorro está desactivado o el dispositivo usa corriente alterna.",
    ),
    ("Reduce motion: {state}", "Reducir movimiento: {state}"),
    ("Refresh", "Actualizar"),
    (
        "Release hidden accounts after",
        "Liberar cuentas ocultas tras",
    ),
    (
        "Release hidden accounts now",
        "Liberar cuentas ocultas ahora",
    ),
    (
        "Released {count} hidden account. Open the account to restore it.",
        "Se liberó {count} cuenta oculta. Abre la cuenta para restaurarla.",
    ),
    (
        "Released {count} hidden accounts. Open the account to restore it.",
        "Se liberaron {count} cuentas ocultas. Abre la cuenta para restaurarla.",
    ),
    (
        "Remember window bounds: {state}",
        "Recordar límites de ventana: {state}",
    ),
    (
        "Remember window size and position",
        "Recordar tamaño y posición de la ventana",
    ),
    ("Remove", "Quitar"),
    ("Remove account", "Quitar cuenta"),
    ("Remove password", "Quitar contraseña"),
    (
        "Remove password protection?",
        "¿Quitar la protección con contraseña?",
    ),
    ("Remove this account?", "¿Quitar esta cuenta?"),
    ("Rename", "Renombrar"),
    (
        "Reopen the media and use the save action to retry.",
        "Vuelve a abrir el medio y usa la acción de guardar para reintentar.",
    ),
    ("Report an issue", "Informar de un problema"),
    (
        "Require a password to unlock",
        "Requerir contraseña para desbloquear",
    ),
    ("Reset", "Restablecer"),
    ("Reset identity", "Restablecer identidad"),
    (
        "Reset preferences to their defaults?",
        "¿Restablecer las preferencias a sus valores predeterminados?",
    ),
    ("Reset settings", "Restablecer ajustes"),
    ("Reset zoom", "Restablecer zoom"),
    (
        "Restore each account window where you left it.",
        "Restaura cada ventana de cuenta donde la dejaste.",
    ),
    ("Restoring WhatsApp Web…", "Restaurando WhatsApp Web…"),
    (
        "Restricted to https://web.whatsapp.com · 64 KiB limit · CSS only · excluded from settings exports.",
        "Limitado a https://web.whatsapp.com · límite de 64 KiB · solo CSS · excluido de las exportaciones de configuración.",
    ),
    (
        "Resume all suspended accounts",
        "Reanudar todas las cuentas suspendidas",
    ),
    ("Retry", "Reintentar"),
    ("Retry connection", "Reintentar conexión"),
    ("Retry now", "Reintentar ahora"),
    ("Retry unavailable", "Reintento no disponible"),
    (
        "Review the WhatsApp Web login, the QR steps, account isolation, tray and close behavior, notifications, the app lock, and where OxideZap stores data.",
        "Repasa el inicio de sesión de WhatsApp Web, los pasos del QR, el aislamiento de cuentas, el comportamiento de la bandeja y el cierre, las notificaciones, el bloqueo de la aplicación y dónde guarda los datos OxideZap.",
    ),
    (
        "Right-click normal media to save it, open it, or copy an image. View-once and protected media are never offered.",
        "Haz clic derecho en medios normales para guardarlos, abrirlos o copiar una imagen. Los medios de ver una vez y protegidos nunca se ofrecen.",
    ),
    (
        "Running storage operation…",
        "Ejecutando operación de almacenamiento…",
    ),
    ("Rust · GPUI · WebKitGTK", "Rust · GPUI · WebKitGTK"),
    ("Save", "Guardar"),
    (
        "Save each account's files in its own subfolder.",
        "Guarda los archivos de cada cuenta en su propia subcarpeta.",
    ),
    (
        "Saved configuration: {summary}",
        "Configuración guardada: {summary}",
    ),
    ("Saved to {path}", "Guardado en {path}"),
    ("Saving to", "Guardando en"),
    ("Scanning…", "Escaneando…"),
    ("Scheduled", "Programado"),
    ("Search commands…", "Buscar comandos…"),
    ("Security", "Seguridad"),
    ("Select", "Seleccionar"),
    ("Selected · {description}", "Seleccionado · {description}"),
    ("Set a password", "Configurar una contraseña"),
    (
        "Set a password in Settings > Security to hide every window behind a privacy screen: lock now, on launch, on hide, on suspend, when the desktop locks, or after an idle timeout.",
        "Configura una contraseña en Ajustes > Seguridad para ocultar todas las ventanas tras una pantalla de privacidad: bloquea ahora, al iniciar, al ocultar, al suspender, cuando se bloquee el escritorio o tras un tiempo de inactividad.",
    ),
    ("Set password", "Configurar contraseña"),
    ("Settings", "Ajustes"),
    ("Settings — {appearance}", "Ajustes — {appearance}"),
    (
        "Settings exported to {path}.",
        "Configuración exportada a {path}.",
    ),
    (
        "Settings imported. The previous settings were backed up to {path}.",
        "Configuración importada. La configuración anterior se guardó en {path}.",
    ),
    ("Settings…", "Ajustes…"),
    ("Show OxideZap", "Mostrar OxideZap"),
    (
        "Show a native notification when a message arrives while OxideZap is in the background.",
        "Muestra una notificación nativa cuando llega un mensaje mientras OxideZap está en segundo plano.",
    ),
    (
        "Show a native notification when an account goes offline or reconnects.",
        "Muestra una notificación nativa cuando una cuenta se desconecta o se reconecta.",
    ),
    (
        "Show all account windows",
        "Mostrar todas las ventanas de cuenta",
    ),
    ("Show or hide OxideZap", "Mostrar u ocultar OxideZap"),
    (
        "Show the first-run guide again",
        "Mostrar la guía inicial de nuevo",
    ),
    ("Sign out", "Cerrar sesión"),
    ("Sign out of “{name}”?", "¿Cerrar sesión en «{name}»?"),
    (
        "Signed out. The account was kept; open it and scan the QR code again.",
        "Sesión cerrada. La cuenta se conservó; ábrela y escanea el código QR de nuevo.",
    ),
    ("Single-phone account", "Cuenta de un solo teléfono"),
    ("Size", "Tamaño"),
    ("Skip", "Omitir"),
    ("Skip this quiet period", "Omitir este periodo de silencio"),
    ("Skip this version", "Omitir esta versión"),
    (
        "Skipping hides this guide permanently.",
        "Omitirla oculta esta guía de forma permanente.",
    ),
    (
        "Sound playback depends on the notification service and its sound theme; some servers ignore the requested sound or always use their own.",
        "La reproducción de sonido depende del servicio de notificaciones y de su tema de sonidos; algunos servidores ignoran el sonido solicitado o siempre usan el suyo.",
    ),
    ("Spanish", "Español"),
    ("Spell check language", "Idioma de corrección ortográfica"),
    ("Start", "Inicio"),
    (
        "Start OxideZap automatically when you sign in to the desktop.",
        "Inicia OxideZap automáticamente al iniciar sesión en el escritorio.",
    ),
    ("Started", "Inicio"),
    ("Status", "Estado"),
    ("Step {step} of {total}", "Paso {step} de {total}"),
    ("Storage", "Almacenamiento"),
    (
        "Suppress native message notifications from this account.",
        "Suprime las notificaciones nativas de mensajes de esta cuenta.",
    ),
    ("Suspended accounts", "Cuentas suspendidas"),
    (
        "Suspending never deletes the account, its WebKit profile, cookies, local storage, or the WhatsApp login. It only destroys the running view, which lets WebKit terminate that account's web process. Downloads, calls, and recordings always block suspension, and nothing is released while the app is locked or quitting.",
        "Suspender nunca elimina la cuenta, su perfil de WebKit, las cookies, el almacenamiento local ni el inicio de sesión de WhatsApp. Solo destruye la vista en ejecución, lo que permite que WebKit termine el proceso web de esa cuenta. Las descargas, llamadas y grabaciones siempre bloquean la suspensión, y no se libera nada mientras la aplicación está bloqueada o cerrando.",
    ),
    (
        "Switch to account {position}: {name}",
        "Cambiar a la cuenta {position}: {name}",
    ),
    (
        "Switch to account {position}: {name} (current)",
        "Cambiar a la cuenta {position}: {name} (actual)",
    ),
    ("System", "Sistema"),
    (
        "System follows the desktop color scheme live. On Linux this depends on the XDG desktop portal and the desktop settings service, so some window managers may not report changes.",
        "Sistema sigue el esquema de color del escritorio en vivo. En Linux esto depende del portal XDG y del servicio de ajustes del escritorio, por lo que algunos gestores de ventanas podrían no notificar cambios.",
    ),
    ("System proxy", "Proxy del sistema"),
    (
        "Tap Linked devices, then Link a device.",
        "Toca Dispositivos vinculados y luego Vincular un dispositivo.",
    ),
    ("Test connection", "Probar conexión"),
    (
        "Test connection checks web.whatsapp.com through the configured route without blocking the interface. HTTP and SOCKS5 proxies are tested end to end; an HTTPS proxy is checked for reachability before the TLS handshake.",
        "Probar conexión comprueba web.whatsapp.com a través de la ruta configurada sin bloquear la interfaz. Los proxies HTTP y SOCKS5 se prueban de extremo a extremo; en un proxy HTTPS se comprueba la accesibilidad antes del handshake TLS.",
    ),
    ("Testing…", "Probando…"),
    (
        "That is everything. Your settings are saved as you change them, and you can reopen this guide any time from Settings > General.",
        "Eso es todo. Tus ajustes se guardan a medida que los cambias y puedes volver a abrir esta guía en cualquier momento desde Ajustes > General.",
    ),
    (
        "The CSS contains a NUL character.",
        "El CSS contiene un carácter NUL.",
    ),
    (
        "The CSS file is too large ({bytes} bytes; the limit is {limit} KiB).",
        "El archivo CSS es demasiado grande ({bytes} bytes; el límite es {limit} KiB).",
    ),
    (
        "The CSS is too large ({bytes} bytes; the limit is {limit} KiB).",
        "El CSS es demasiado grande ({bytes} bytes; el límite es {limit} KiB).",
    ),
    (
        "The CSS must not contain a </style> tag.",
        "El CSS no debe contener una etiqueta </style>.",
    ),
    (
        "The CSS must not contain javascript: URLs.",
        "El CSS no debe contener URL javascript:.",
    ),
    (
        "The CSS must not load remote resources. Use data: URIs or local rules instead.",
        "El CSS no debe cargar recursos remotos. Usa URI data: o reglas locales.",
    ),
    (
        "The CSS must not use @import. Paste the rules directly instead.",
        "El CSS no debe usar @import. Pega las reglas directamente.",
    ),
    (
        "The HTTP proxy at {endpoint} accepted a CONNECT tunnel to web.whatsapp.com:443.",
        "El proxy HTTP en {endpoint} aceptó un túnel CONNECT a web.whatsapp.com:443.",
    ),
    (
        "The HTTPS proxy at {endpoint} is reachable. The test stops before the TLS handshake; opening an account verifies the full connection.",
        "El proxy HTTPS en {endpoint} es accesible. La prueba se detiene antes del handshake TLS; abrir una cuenta verifica la conexión completa.",
    ),
    (
        "The SOCKS proxy at {endpoint} connected to web.whatsapp.com:443.",
        "El proxy SOCKS en {endpoint} se conectó a web.whatsapp.com:443.",
    ),
    (
        "The SOCKS proxy connection timed out.",
        "La conexión al proxy SOCKS agotó el tiempo de espera.",
    ),
    (
        "The SOCKS proxy could not reach the network.",
        "El proxy SOCKS no pudo llegar a la red.",
    ),
    (
        "The SOCKS proxy could not reach web.whatsapp.com.",
        "El proxy SOCKS no pudo llegar a web.whatsapp.com.",
    ),
    (
        "The SOCKS proxy denied the connection.",
        "El proxy SOCKS denegó la conexión.",
    ),
    (
        "The SOCKS proxy did not accept username and password authentication.",
        "El proxy SOCKS no aceptó la autenticación con usuario y contraseña.",
    ),
    (
        "The SOCKS proxy did not answer CONNECT: {error}",
        "El proxy SOCKS no respondió a CONNECT: {error}",
    ),
    (
        "The SOCKS proxy did not answer the login: {error}",
        "El proxy SOCKS no respondió al inicio de sesión: {error}",
    ),
    (
        "The SOCKS proxy did not answer: {error}",
        "El proxy SOCKS no respondió: {error}",
    ),
    (
        "The SOCKS proxy does not support CONNECT.",
        "El proxy SOCKS no admite CONNECT.",
    ),
    (
        "The SOCKS proxy does not support this address type.",
        "El proxy SOCKS no admite este tipo de dirección.",
    ),
    (
        "The SOCKS proxy refused the connection to web.whatsapp.com.",
        "El proxy SOCKS rechazó la conexión a web.whatsapp.com.",
    ),
    (
        "The SOCKS proxy rejected the offered authentication methods.",
        "El proxy SOCKS rechazó los métodos de autenticación ofrecidos.",
    ),
    (
        "The SOCKS proxy rejected the username or password.",
        "El proxy SOCKS rechazó el usuario o la contraseña.",
    ),
    (
        "The SOCKS proxy reported a general failure.",
        "El proxy SOCKS informó de un fallo general.",
    ),
    (
        "The SOCKS proxy requires authentication.",
        "El proxy SOCKS requiere autenticación.",
    ),
    (
        "The SOCKS proxy returned an unknown address type.",
        "El proxy SOCKS devolvió un tipo de dirección desconocido.",
    ),
    (
        "The SOCKS proxy returned an unknown error.",
        "El proxy SOCKS devolvió un error desconocido.",
    ),
    (
        "The SOCKS username and password must each be at most 255 bytes.",
        "El usuario y la contraseña de SOCKS deben tener como máximo 255 bytes cada uno.",
    ),
    (
        "The account and its settings are kept, but its WebKit data is deleted. WhatsApp Web will ask for the QR code again.",
        "La cuenta y sus ajustes se conservan, pero sus datos de WebKit se eliminan. WhatsApp Web volverá a pedir el código QR.",
    ),
    (
        "The account window behind this guide shows WhatsApp Web's QR code. Keep the guide open while you pick up your phone, or skip it and come back from Settings > General later.",
        "La ventana de la cuenta detrás de esta guía muestra el código QR de WhatsApp Web. Mantén la guía abierta mientras coges el teléfono, u omítela y vuelve más tarde desde Ajustes > General.",
    ),
    (
        "The app lock uses Argon2id. System suspend and screen-lock detection are best effort through D-Bus.",
        "El bloqueo de la aplicación usa Argon2id. La detección de suspensión del sistema y de bloqueo de pantalla es de mejor esfuerzo mediante D-Bus.",
    ),
    (
        "The boss key must differ from the command palette shortcut.",
        "El atajo de jefe debe ser distinto del atajo de la paleta de comandos.",
    ),
    (
        "The boss key must differ from the show/hide shortcut.",
        "El atajo de jefe debe ser distinto del atajo de mostrar/ocultar.",
    ),
    (
        "The boss key uses the same X11/XWayland grab and can conflict with desktop shortcuts.",
        "El atajo de jefe usa la misma captura X11/XWayland y puede entrar en conflicto con atajos del escritorio.",
    ),
    (
        "The command palette shortcut must differ from the boss key.",
        "El atajo de la paleta de comandos debe ser distinto del atajo de jefe.",
    ),
    (
        "The command palette shortcut must differ from the show/hide shortcut.",
        "El atajo de la paleta de comandos debe ser distinto del atajo de mostrar/ocultar.",
    ),
    (
        "The default show/hide shortcut is Ctrl+Shift+W. The boss key, Ctrl+Shift+H, hides every window immediately for privacy and restores them on the next press.",
        "El atajo predeterminado de mostrar/ocultar es Ctrl+Shift+W. El atajo de jefe, Ctrl+Shift+H, oculta todas las ventanas de inmediato por privacidad y las restaura al pulsarlo de nuevo.",
    ),
    (
        "The desktop proxy settings contain an invalid proxy URI.",
        "La configuración del proxy del escritorio contiene una URI de proxy no válida.",
    ),
    (
        "The desktop proxy URI has no host.",
        "La URI del proxy del escritorio no tiene host.",
    ),
    (
        "The desktop proxy URI uses port 0.",
        "La URI del proxy del escritorio usa el puerto 0.",
    ),
    (
        "The download history keeps the last {count} entries. Existing files are never overwritten; collisions get a numbered suffix.",
        "El historial de descargas conserva las últimas {count} entradas. Los archivos existentes nunca se sobrescriben; las colisiones reciben un sufijo numérico.",
    ),
    (
        "The estimate assumes about {size} per released WebKit web process. WebKit does not expose per-view memory, so this is not a measurement.",
        "La estimación supone unos {size} por cada proceso web de WebKit liberado. WebKit no expone la memoria por vista, así que no es una medición.",
    ),
    (
        "The file is not a valid OxideZap settings export.",
        "El archivo no es una exportación de configuración de OxideZap válida.",
    ),
    (
        "The image is on the clipboard.",
        "La imagen está en el portapapeles.",
    ),
    (
        "The local WhatsApp session, cookies, and cached files for this account will be deleted. This cannot be undone.",
        "La sesión local de WhatsApp, las cookies y los archivos en caché de esta cuenta se eliminarán. Esto no se puede deshacer.",
    ),
    ("The official login", "El inicio de sesión oficial"),
    (
        "The page kept crashing. Your session and messages are safe. Retry when you are ready.",
        "La página seguía fallando. Tu sesión y tus mensajes están a salvo. Reinténtalo cuando quieras.",
    ),
    (
        "The password is stored as an Argon2id hash in your config directory.",
        "La contraseña se guarda como un hash Argon2id en tu directorio de configuración.",
    ),
    (
        "The passwords do not match.",
        "Las contraseñas no coinciden.",
    ),
    (
        "The proxy address is not a valid URL.",
        "La dirección del proxy no es una URL válida.",
    ),
    (
        "The proxy did not answer the CONNECT request: {error}",
        "El proxy no respondió a la solicitud CONNECT: {error}",
    ),
    (
        "The proxy did not speak SOCKS5.",
        "El proxy no habla SOCKS5.",
    ),
    (
        "The proxy host must be a hostname or IP address, without a scheme, path, or credentials.",
        "El host del proxy debe ser un nombre de host o una dirección IP, sin esquema, ruta ni credenciales.",
    ),
    (
        "The proxy is applied to each account's WebKit data manager when the account opens. Existing connections keep their route, so reopen an account if the change does not take effect.",
        "El proxy se aplica al gestor de datos de WebKit de cada cuenta cuando esta se abre. Las conexiones existentes conservan su ruta, así que reabre una cuenta si el cambio no surte efecto.",
    ),
    (
        "The proxy port must be a number between 1 and 65535.",
        "El puerto del proxy debe ser un número entre 1 y 65535.",
    ),
    (
        "The proxy port must be between 1 and 65535.",
        "El puerto del proxy debe estar entre 1 y 65535.",
    ),
    (
        "The proxy rejected the credentials (HTTP 407).",
        "El proxy rechazó las credenciales (HTTP 407).",
    ),
    (
        "The proxy requires a username and password (HTTP 407).",
        "El proxy requiere un usuario y una contraseña (HTTP 407).",
    ),
    (
        "The proxy returned HTTP {status} while opening the tunnel to web.whatsapp.com.",
        "El proxy devolvió HTTP {status} al abrir el túnel a web.whatsapp.com.",
    ),
    (
        "The proxy returned an unexpected response to CONNECT.",
        "El proxy devolvió una respuesta inesperada a CONNECT.",
    ),
    (
        "The quiet-hours start and end times must differ.",
        "Las horas de inicio y fin del silencio deben ser distintas.",
    ),
    (
        "The settings export is too large ({bytes} bytes; the limit is {limit} bytes).",
        "La exportación de configuración es demasiado grande ({bytes} bytes; el límite es {limit} bytes).",
    ),
    (
        "The show/hide shortcut must differ from the boss key.",
        "El atajo de mostrar/ocultar debe ser distinto del atajo de jefe.",
    ),
    (
        "The show/hide shortcut must differ from the command palette shortcut.",
        "El atajo de mostrar/ocultar debe ser distinto del atajo de la paleta de comandos.",
    ),
    ("Text files (*.txt)", "Archivos de texto (*.txt)"),
    ("Text size", "Tamaño del texto"),
    (
        "Text size: {from}% -> {to}%",
        "Tamaño del texto: {from}% -> {to}%",
    ),
    (
        "Text size scales the Settings window, the lock screen, and the native text in account windows. Icons, spacing, window chrome, text-field contents, the command palette, and the other native windows keep their fixed sizes; WhatsApp Web zoom is separate.",
        "El tamaño del texto escala la ventana de Ajustes, la pantalla de bloqueo y el texto nativo de las ventanas de cuenta. Los iconos, el espaciado, el marco de la ventana, el contenido de los campos de texto, la paleta de comandos y las demás ventanas nativas mantienen sus tamaños fijos; el zoom de WhatsApp Web es independiente.",
    ),
    ("Theme", "Tema"),
    (
        "Theme, zoom, notifications, startup, the global shortcut, the app-lock password, and the first-run guide status are reset. Accounts and their WhatsApp sessions are kept, and the first-run guide opens again. A dated backup is written first.",
        "Se restablecen el tema, el zoom, las notificaciones, el inicio, el atajo global, la contraseña del bloqueo y el estado de la guía inicial. Las cuentas y sus sesiones de WhatsApp se conservan, y la guía inicial se abre de nuevo. Antes se escribe una copia de seguridad con fecha.",
    ),
    ("Theme: {from} -> {to}", "Tema: {from} -> {to}"),
    ("This account: {appearance}", "Esta cuenta: {appearance}"),
    (
        "This closes the account, deletes its WebKit profile, and removes it from OxideZap. This cannot be undone.",
        "Esto cierra la cuenta, elimina su perfil de WebKit y la quita de OxideZap. No se puede deshacer.",
    ),
    (
        "This download has no reusable source URL. Start it again from WhatsApp Web.",
        "Esta descarga no tiene una URL de origen reutilizable. Iníciala de nuevo desde WhatsApp Web.",
    ),
    (
        "This guide makes no network requests. The app collects no analytics and never asks for your phone number or WhatsApp credentials.",
        "Esta guía no hace solicitudes de red. La aplicación no recopila analíticas y nunca pide tu número de teléfono ni tus credenciales de WhatsApp.",
    ),
    (
        "This text is shown at {percent}% of its base size.",
        "Este texto se muestra al {percent}% de su tamaño base.",
    ),
    ("Total account data", "Datos totales de las cuentas"),
    (
        "Translates the native shell and its windows. WhatsApp Web content keeps its own language.",
        "Traduce la carcasa nativa y sus ventanas. El contenido de WhatsApp Web conserva su propio idioma.",
    ),
    ("Try a different search.", "Prueba con otra búsqueda."),
    ("Turn off Do Not Disturb", "Desactivar No molestar"),
    ("Turn on Do Not Disturb", "Activar No molestar"),
    (
        "Type the account name exactly to confirm.",
        "Escribe el nombre de la cuenta exactamente para confirmar.",
    ),
    (
        "Type the account name to confirm",
        "Escribe el nombre de la cuenta para confirmar",
    ),
    (
        "Unread count in window title",
        "Contador sin leer en el título de la ventana",
    ),
    (
        "Prefix the title with the account's unread count, for example (3) OxideZap — Work.",
        "Prefija el título con el contador sin leer de la cuenta, por ejemplo (3) OxideZap — Work.",
    ),
    ("Unavailable", "No disponible"),
    ("Unlock", "Desbloquear"),
    (
        "Unsupported and experimental",
        "No compatible y experimental",
    ),
    (
        "Unsupported settings export version {version}; this build supports version {supported}.",
        "Versión de exportación de configuración no compatible {version}; esta compilación admite la versión {supported}.",
    ),
    ("Until I turn it off", "Hasta que lo desactive"),
    ("Until tomorrow ({wake})", "Hasta mañana ({wake})"),
    ("Update password", "Actualizar contraseña"),
    (
        "Use WebKitGTK's local spell checker in the WhatsApp composer.",
        "Usa el corrector ortográfico local de WebKitGTK en el editor de WhatsApp.",
    ),
    ("Use dark theme", "Usar tema oscuro"),
    ("Use default", "Usar predeterminada"),
    ("Use global", "Usar configuración global"),
    ("Use global ({value})", "Usar global ({value})"),
    ("Use light theme", "Usar tema claro"),
    (
        "Use modifiers plus one key, for example Ctrl+Shift+W.",
        "Usa modificadores más una tecla, por ejemplo Ctrl+Shift+W.",
    ),
    (
        "Use the desktop network settings",
        "Usar la configuración de red del escritorio",
    ),
    (
        "Use up to two characters or one emoji. Empty shows the account initial.",
        "Usa hasta dos caracteres o un emoji. Vacío muestra la inicial de la cuenta.",
    ),
    (
        "Use up to two characters or one emoji.",
        "Usa hasta dos caracteres o un emoji.",
    ),
    ("Username", "Usuario"),
    ("Version {version}", "Versión {version}"),
    (
        "Version {version} is available",
        "La versión {version} está disponible",
    ),
    (
        "Version {version} is skipped.",
        "La versión {version} está omitida.",
    ),
    ("View release", "Ver versión"),
    ("View-once messages", "Mensajes de ver una vez"),
    (
        "Waiting for the file dialog or the current operation…",
        "Esperando al diálogo de archivos o a la operación actual…",
    ),
    (
        "Waiting for the file dialog…",
        "Esperando al diálogo de archivos…",
    ),
    ("Weekdays", "Entre semana"),
    ("Weekends", "Fines de semana"),
    ("Welcome to OxideZap", "Te damos la bienvenida a OxideZap"),
    ("What suspension keeps", "Qué conserva la suspensión"),
    (
        "WhatsApp Web changes its DOM without notice. A stylesheet can break the QR login, calls, dialogs, or the chat view, and may stop working after any WhatsApp update. Do not hide security, privacy, or abuse-reporting controls. OxideZap injects CSS only: it never runs JavaScript from this file and never downloads remote themes.",
        "WhatsApp Web cambia su DOM sin avisar. Una hoja de estilos puede romper el inicio de sesión por QR, las llamadas, los diálogos o la vista de chat, y puede dejar de funcionar tras cualquier actualización de WhatsApp. No ocultes controles de seguridad, privacidad ni denuncia de abuso. OxideZap solo inyecta CSS: nunca ejecuta JavaScript de este archivo ni descarga temas remotos.",
    ),
    (
        "WhatsApp Web could not start",
        "WhatsApp Web no pudo iniciarse",
    ),
    (
        "WhatsApp Web crashed",
        "WhatsApp Web se cerró inesperadamente",
    ),
    ("WhatsApp Web for Linux", "WhatsApp Web para Linux"),
    (
        "WhatsApp Web is not running, so its memory was released. Your login, chats, and settings stay on disk.",
        "WhatsApp Web no se está ejecutando, así que se liberó su memoria. Tu inicio de sesión, chats y ajustes permanecen en disco.",
    ),
    ("WhatsApp Web is offline", "WhatsApp Web está desconectado"),
    (
        "WhatsApp Web is offline.",
        "WhatsApp Web está desconectado.",
    ),
    (
        "WhatsApp Web notifications are delivered by the desktop notification service.",
        "Las notificaciones de WhatsApp Web las entrega el servicio de notificaciones del escritorio.",
    ),
    (
        "WhatsApp Web zoom is limited to {min}%–{max}%. It does not resize the native UI; use Accessibility for native text size.",
        "El zoom de WhatsApp Web está limitado a {min}%–{max}%. No cambia el tamaño de la interfaz nativa; usa Accesibilidad para el tamaño del texto nativo.",
    ),
    (
        "WhatsApp keeps handling the account, the QR login, and your messages. OxideZap only provides the desktop window around it.",
        "WhatsApp sigue gestionando la cuenta, el inicio de sesión por QR y tus mensajes. OxideZap solo proporciona la ventana de escritorio que lo rodea.",
    ),
    (
        "WhatsApp only lets you open view-once media on your phone. OxideZap does not bypass that restriction; the desktop window follows WhatsApp Web's own rules.",
        "WhatsApp solo permite abrir medios de ver una vez en tu teléfono. OxideZap no elude esa restricción; la ventana de escritorio sigue las propias reglas de WhatsApp Web.",
    ),
    ("Where your data lives", "Dónde viven tus datos"),
    ("Window", "Ventana"),
    (
        "Window behavior, startup, writing, downloads, media actions, and the global show/hide shortcut.",
        "Comportamiento de ventanas, inicio, escritura, descargas, acciones de medios y el atajo global de mostrar/ocultar.",
    ),
    ("Window open", "Ventana abierta"),
    ("Windows and privacy", "Ventanas y privacidad"),
    (
        "Wrap IPv6 addresses in brackets, for example [::1].",
        "Escribe las direcciones IPv6 entre corchetes, por ejemplo [::1].",
    ),
    ("Writing", "Escritura"),
    (
        "You are running the latest release.",
        "Estás usando la última versión.",
    ),
    ("You're ready", "Todo listo"),
    (
        "Your session stays on this computer",
        "Tu sesión permanece en este equipo",
    ),
    ("Zoom in", "Acercar"),
    ("Zoom out", "Alejar"),
    ("Zoom: {from}% -> {to}%", "Zoom: {from}% -> {to}%"),
    ("applied", "aplicado"),
    ("clear the cache", "borrar la caché"),
    ("delete the account data", "eliminar los datos de la cuenta"),
    ("every day", "todos los días"),
    ("less than 1m left", "queda menos de 1 min"),
    (
        "no active account; pass --account",
        "no hay una cuenta activa; pasa --account",
    ),
    (
        "no address was returned",
        "no se devolvió ninguna dirección",
    ),
    ("no days", "ningún día"),
    ("not applied", "no aplicado"),
    ("not configured", "sin configurar"),
    ("off", "desactivado"),
    ("on", "activado"),
    ("on until turned off", "activado hasta que se desactive"),
    ("quiet hours {schedule}", "silencio {schedule}"),
    (
        "release hidden accounts after {minutes} min",
        "liberar cuentas ocultas tras {minutes} min",
    ),
    ("sign out", "cerrar sesión"),
    ("suspended", "suspendida"),
    ("the HTTP proxy", "el proxy HTTP"),
    ("the HTTPS proxy", "el proxy HTTPS"),
    ("the SOCKS proxy", "el proxy SOCKS"),
    (
        "the application is locked; unlock it first",
        "la aplicación está bloqueada; desbloquéala primero",
    ),
    (
        "the application lock is not configured",
        "el bloqueo de la aplicación no está configurado",
    ),
    ("the account no longer exists", "la cuenta ya no existe"),
    (
        "the last account cannot be deleted",
        "la última cuenta no se puede eliminar",
    ),
    ("Enter a shortcut.", "Introduce un atajo."),
    ("Add a key to the shortcut.", "Añade una tecla al atajo."),
    (
        "A global shortcut needs at least one modifier.",
        "Un atajo global necesita al menos un modificador.",
    ),
    (
        "That key is not supported for global shortcuts.",
        "Esa tecla no es compatible con los atajos globales.",
    ),
    ("tomorrow {time}", "mañana {time}"),
    ("weekdays", "entre semana"),
    ("weekends", "fines de semana"),
    ("{active} of {total}", "{active} de {total}"),
    ("{count} command", "{count} comando"),
    ("{count} commands", "{count} comandos"),
    ("{count} item", "{count} elemento"),
    ("{count} items", "{count} elementos"),
    (
        "{count} items · {active} in progress",
        "{count} elementos · {active} en curso",
    ),
    ("{count} message", "{count} mensaje"),
    ("{count} messages", "{count} mensajes"),
    ("{count} suspended account", "{count} cuenta suspendida"),
    ("{count} suspended accounts", "{count} cuentas suspendidas"),
    ("{count} unread message", "{count} mensaje sin leer"),
    ("{count} unread messages", "{count} mensajes sin leer"),
    ("{days}d left", "quedan {days} d"),
    ("{days}d {hours}h left", "quedan {days} d {hours} h"),
    (
        "{filename}\nSaved to {destination}",
        "{filename}\nGuardado en {destination}",
    ),
    (
        "{filename}\nSaving to {folder}",
        "{filename}\nGuardando en {folder}",
    ),
    ("{filename}\n{hint}", "{filename}\n{hint}"),
    ("{filename}\n{reason}", "{filename}\n{reason}"),
    ("{hours} h ago", "hace {hours} h"),
    ("{hours}h left", "quedan {hours} h"),
    ("{hours}h {minutes}m left", "quedan {hours} h {minutes} min"),
    ("{minutes} min ago", "hace {minutes} min"),
    ("{minutes}m left", "quedan {minutes} min"),
    (
        "{name} is connected again.",
        "{name} vuelve a estar conectada.",
    ),
    (
        "{name} is on the WhatsApp login page. OxideZap did not reload it automatically; use Retry now if the page is stuck.",
        "{name} está en la página de inicio de sesión de WhatsApp. OxideZap no la recargó automáticamente; usa Reintentar ahora si la página está atascada.",
    ),
    (
        "{name} lost its connection. OxideZap will reconnect when the network returns.",
        "{name} perdió su conexión. OxideZap se reconectará cuando vuelva la red.",
    ),
    ("{path} is not valid UTF-8.", "{path} no es UTF-8 válido."),
    (
        "{percent}% · {received} of {total}",
        "{percent}% · {received} de {total}",
    ),
    (
        "{protocol} proxy at {endpoint}",
        "proxy {protocol} en {endpoint}",
    ),
    (
        "{result} Resolved from the desktop system proxy settings.",
        "{result} Resuelto desde la configuración del proxy del sistema del escritorio.",
    ),
    ("{size} · {state}", "{size} · {state}"),
    ("{theme} · {zoom}%", "{theme} · {zoom}%"),
    (
        "↑/↓ Move · Enter Run · Esc Close",
        "↑/↓ Mover · Enter Ejecutar · Esc Cerrar",
    ),
    ("Hidden", "Oculto"),
    ("Full preview", "Vista previa completa"),
    (
        "Hide previews when locked: {state}",
        "Ocultar vistas previas al bloquear: {state}",
    ),
    (
        "Hide previews while OxideZap is locked",
        "Ocultar vistas previas mientras OxideZap está bloqueado",
    ),
    ("Message from {sender}", "Mensaje de {sender}"),
    ("New WhatsApp message", "Nuevo mensaje de WhatsApp"),
    ("Notification privacy", "Privacidad de notificaciones"),
    (
        "Notification privacy controls what the native notification shows. Unread counts, window titles, the tray badge, and the click target never change.",
        "La privacidad de notificaciones controla qué muestra la notificación nativa. Los contadores sin leer, los títulos de ventana, la insignia de la bandeja y el destino del clic nunca cambian.",
    ),
    (
        "Notification privacy: {from} -> {to}",
        "Privacidad de notificaciones: {from} -> {to}",
    ),
    (
        "Open OxideZap for details.",
        "Abre OxideZap para ver los detalles.",
    ),
    ("Open OxideZap to read it.", "Abre OxideZap para leerlo."),
    ("Preview", "Vista previa"),
    ("See you at 18:00.", "Nos vemos a las 18:00."),
    ("Sender only", "Solo remitente"),
    (
        "Sender only falls back to Hidden when WhatsApp does not provide a sender.",
        "Solo remitente recurre a Oculto cuando WhatsApp no proporciona un remitente.",
    ),
    (
        "Shows a generic message with no sender or content.",
        "Muestra un mensaje genérico sin remitente ni contenido.",
    ),
    (
        "Shows the sender and the message text.",
        "Muestra el remitente y el texto del mensaje.",
    ),
    (
        "Shows the sender name but hides the message text.",
        "Muestra el nombre del remitente pero oculta el texto del mensaje.",
    ),
    (
        "The notification daemon and the operating system may still store or show content on their own lock screen; OxideZap cannot control that.",
        "El demonio de notificaciones y el sistema operativo pueden seguir guardando o mostrando contenido en su propia pantalla de bloqueo; OxideZap no puede controlarlo.",
    ),
    (
        "The selected level applies to message and system notifications such as downloads and errors. Connection and crash notifications keep their full text.",
        "El nivel seleccionado se aplica a los mensajes y a las notificaciones del sistema, como descargas y errores. Las notificaciones de conexión y de fallos conservan su texto completo.",
    ),
    (
        "This is what a message notification looks like with the current selection.",
        "Así se ve una notificación de mensaje con la selección actual.",
    ),
    (
        "While the app is locked, notifications use the Hidden level regardless of the selection above.",
        "Mientras la aplicación está bloqueada, las notificaciones usan el nivel Oculto independientemente de la selección anterior.",
    ),
];

/// Spanish translations keyed by the English source text.
fn spanish_translation(key: &str) -> Option<&'static str> {
    match key {
        " · partial" => Some(" · parcial"),
        "'Until tomorrow' uses the quiet-hours end time as its wake hour. Changing the times requires Apply; toggles and presets apply immediately." => {
            Some(
                "'Hasta mañana' usa la hora de fin del horario de silencio como hora de reactivación. Cambiar las horas requiere Aplicar; los interruptores y preajustes se aplican de inmediato.",
            )
        }
        "A WhatsApp account stays linked to your phone. OxideZap does not run a second copy of the account and cannot receive messages while the phone is offline, just like WhatsApp Web." => {
            Some(
                "Una cuenta de WhatsApp permanece vinculada a tu teléfono. OxideZap no ejecuta una segunda copia de la cuenta y no puede recibir mensajes mientras el teléfono está desconectado, igual que WhatsApp Web.",
            )
        }
        "A global palette shortcut takes precedence over WhatsApp Web's own Ctrl+K chat search." => {
            Some(
                "Un atajo global de la paleta tiene prioridad sobre la búsqueda de chats Ctrl+K de WhatsApp Web.",
            )
        }
        "A native Linux shell around the official WhatsApp Web experience." => {
            Some("Una carcasa nativa de Linux alrededor de la experiencia oficial de WhatsApp Web.")
        }
        "A newer release is published on GitHub. OxideZap does not download or install it automatically." => {
            Some(
                "Hay una versión más reciente publicada en GitHub. OxideZap no la descarga ni la instala automáticamente.",
            )
        }
        "A second global shortcut for instant privacy. A second press restores the windows that were visible." => {
            Some(
                "Un segundo atajo global para privacidad instantánea. Al pulsarlo de nuevo se restauran las ventanas que estaban visibles.",
            )
        }
        "About" => Some("Acerca de"),
        "About OxideZap" => Some("Acerca de OxideZap"),
        "Accessibility" => Some("Accesibilidad"),
        "Identity" => Some("Identidad"),
        "Use a hex color such as #1d4ed8." => Some("Usa un color hexadecimal como #1d4ed8."),
        "Use up to two characters or one emoji. Empty shows the account initial." => {
            Some("Usa hasta dos caracteres o un emoji. Vacío muestra la inicial de la cuenta.")
        }
        "Use up to two characters or one emoji." => Some("Usa hasta dos caracteres o un emoji."),
        "Account" => Some("Cuenta"),
        "Account appearance" => Some("Apariencia de la cuenta"),
        "Account data" => Some("Datos de la cuenta"),
        "Account isolation" => Some("Aislamiento de cuentas"),
        "Account name" => Some("Nombre de la cuenta"),
        "Account suspended" => Some("Cuenta suspendida"),
        "Accounts" => Some("Cuentas"),
        "Accounts and data" => Some("Cuentas y datos"),
        "Accounts from the file replace the current account list; local session data is not deleted. The app-lock password is not imported, so set it again afterwards." => {
            Some(
                "Las cuentas del archivo reemplazan la lista actual; los datos de sesión locales no se eliminan. La contraseña del bloqueo no se importa, así que vuelve a configurarla después.",
            )
        }
        "Accounts: {from} -> {to}" => Some("Cuentas: {from} -> {to}"),
        "Actions" => Some("Acciones"),
        "Active views" => Some("Vistas activas"),
        "Add CSS before enabling experimental styles." => {
            Some("Añade CSS antes de activar los estilos experimentales.")
        }
        "Add a YYYY-MM subfolder (UTC) for the month each file was downloaded." => {
            Some("Añade una subcarpeta AAAA-MM (UTC) para el mes en que se descargó cada archivo.")
        }
        "Add account" => Some("Añadir cuenta"),
        "Add account…" => Some("Añadir cuenta…"),
        "Adding an account opens a new window with a fresh QR code." => {
            Some("Añadir una cuenta abre una ventana nueva con un código QR nuevo.")
        }
        "Always connect directly" => Some("Conectar siempre directamente"),
        "Another storage operation is already running." => {
            Some("Ya hay otra operación de almacenamiento en curso.")
        }
        "App" => Some("Aplicación"),
        "App lock" => Some("Bloqueo de la aplicación"),
        "Appearance" => Some("Apariencia"),
        "Appearance applied to all accounts." => Some("Apariencia aplicada a todas las cuentas."),
        "Apply boss key" => Some("Aplicar atajo de jefe"),
        "Apply experimental CSS" => Some("Aplicar CSS experimental"),
        "Apply folder" => Some("Aplicar carpeta"),
        "Apply import" => Some("Aplicar importación"),
        "Apply palette shortcut" => Some("Aplicar atajo de la paleta"),
        "Apply proxy settings" => Some("Aplicar ajustes del proxy"),
        "Apply quiet hours" => Some("Aplicar horario de silencio"),
        "Apply shortcut" => Some("Aplicar atajo"),
        "Apply this account's appearance to all accounts?" => {
            Some("¿Aplicar la apariencia de esta cuenta a todas las cuentas?")
        }
        "Apply to all accounts" => Some("Aplicar a todas las cuentas"),
        "Ask GitHub for the latest release at most once every 24 hours. The request contains no personal data." => {
            Some(
                "Consulta a GitHub la última versión como máximo una vez cada 24 horas. La solicitud no contiene datos personales.",
            )
        }
        "Ask for the password when OxideZap starts." => {
            Some("Pide la contraseña cuando OxideZap se inicia.")
        }
        "Ask the desktop notification service to play a message sound." => Some(
            "Pide al servicio de notificaciones del escritorio que reproduzca un sonido de mensaje.",
        ),
        "At least {count} characters." => Some("Al menos {count} caracteres."),
        "Use a stronger palette for text, backgrounds, and borders in the native shell. The System, Light, and Dark choice still picks the base palette; WhatsApp Web content is unchanged." => {
            Some(
                "Usa una paleta con más contraste para el texto, los fondos y los bordes de la carcasa nativa. La opción Sistema, Claro u Oscuro sigue eligiendo la paleta base; el contenido de WhatsApp Web no cambia.",
            )
        }
        "Use at least {count} characters." => Some("Usa al menos {count} caracteres."),
        "Automatic" => Some("Automático"),
        "Automatic follows the desktop locale. WebKitGTK only checks languages whose hunspell dictionaries are installed. Changes apply to open accounts; WhatsApp Web still controls its own composer and Enter-to-send behavior." => {
            Some(
                "Automático sigue la configuración regional del escritorio. WebKitGTK solo revisa los idiomas cuyos diccionarios hunspell están instalados. Los cambios se aplican a las cuentas abiertas; WhatsApp Web sigue controlando su propio editor y el comportamiento de Enter para enviar.",
            )
        }
        "Automatic recovery failed after {attempts} attempts. Use “Retry now” in the account window." => {
            Some(
                "La recuperación automática falló tras {attempts} intentos. Usa «Reintentar ahora» en la ventana de la cuenta.",
            )
        }
        "Automatic update checks are off." => {
            Some("Las comprobaciones automáticas de actualizaciones están desactivadas.")
        }
        "Avatar" => Some("Avatar"),
        "Back" => Some("Atrás"),
        "Backup and restore" => Some("Copia de seguridad y restauración"),
        "Best effort: depends on the desktop exposing a ScreenSaver D-Bus interface." => Some(
            "Mejor esfuerzo: depende de que el escritorio exponga una interfaz D-Bus de ScreenSaver.",
        ),
        "Boss key" => Some("Atajo de jefe"),
        "Boss key: {from} -> {to}" => Some("Atajo de jefe: {from} -> {to}"),
        "Bright surfaces" => Some("Superficies claras"),
        "Built with" => Some("Creado con"),
        "CSS files (*.css)" => Some("Archivos CSS (*.css)"),
        "CSS loaded and applied to open accounts." => {
            Some("CSS cargado y aplicado a las cuentas abiertas.")
        }
        "CSS loaded. Use the toggle above to apply it." => {
            Some("CSS cargado. Usa el interruptor de arriba para aplicarlo.")
        }
        "Cache cleared ({freed} freed). The WhatsApp session was kept." => {
            Some("Caché borrada ({freed} liberados). Se conservó la sesión de WhatsApp.")
        }
        "Cancel" => Some("Cancelar"),
        "Cancelled" => Some("Cancelada"),
        "Change password" => Some("Cambiar contraseña"),
        "Check for updates automatically" => Some("Buscar actualizaciones automáticamente"),
        "Choose a local .css file to load a stylesheet." => {
            Some("Elige un archivo .css local para cargar una hoja de estilos.")
        }
        "Choose how WhatsApp Web reaches the network. Changes apply per account and may require reopening an account." => {
            Some(
                "Elige cómo accede WhatsApp Web a la red. Los cambios se aplican por cuenta y pueden requerir reabrir una cuenta.",
            )
        }
        "Choose how the native shell looks and how large WhatsApp Web is rendered." => {
            Some("Elige cómo se ve la carcasa nativa y con qué tamaño se renderiza WhatsApp Web.")
        }
        "Choose the OxideZap download folder" => Some("Elige la carpeta de descargas de OxideZap"),
        "Choose…" => Some("Elegir…"),
        "Clear cache" => Some("Borrar caché"),
        "Clearing data closes and recreates the account's webview. Clearing the cache keeps the WhatsApp session; signing out requires the QR code again. Sizes are approximate while WhatsApp Web is running and stop at a bounded scan limit." => {
            Some(
                "Borrar datos cierra y recrea la vista web de la cuenta. Borrar la caché conserva la sesión de WhatsApp; cerrar sesión requiere el código QR de nuevo. Los tamaños son aproximados mientras WhatsApp Web está en ejecución y se detienen en un límite de escaneo acotado.",
            )
        }
        "Close" => Some("Cerrar"),
        "Close the IPv6 address with a bracket, for example [::1]." => {
            Some("Cierra la dirección IPv6 con un corchete, por ejemplo [::1].")
        }
        "Close to tray" => Some("Cerrar a la bandeja"),
        "Close to tray: {state}" => Some("Cerrar a la bandeja: {state}"),
        "Closing an account window hides it instead of quitting OxideZap." => {
            Some("Cerrar una ventana de cuenta la oculta en lugar de salir de OxideZap.")
        }
        "Closing an account window hides it instead of quitting. Reopen it from the tray icon or the global show/hide shortcut; turn this off in Settings > General." => {
            Some(
                "Cerrar una ventana de cuenta la oculta en lugar de salir. Vuelve a abrirla desde el icono de la bandeja o el atajo global de mostrar/ocultar; desactívalo en Ajustes > General.",
            )
        }
        "Command palette" => Some("Paleta de comandos"),
        "Command palette (Ctrl+K)" => Some("Paleta de comandos (Ctrl+K)"),
        "Command palette shortcut: {from} -> {to}" => {
            Some("Atajo de la paleta de comandos: {from} -> {to}")
        }
        "Complete" => Some("Completo"),
        "Completed" => Some("Completada"),
        "Completed (guide version {version}). Reopening it here never marks it uncompleted." => {
            Some(
                "Completada (versión de la guía {version}). Volver a abrirla aquí nunca la marca como incompleta.",
            )
        }
        "Config" => Some("Configuración"),
        "Confirm password" => Some("Confirmar contraseña"),
        "Connected" => Some("Conectado"),
        "Connected directly to web.whatsapp.com:443. No proxy is configured." => {
            Some("Conexión directa a web.whatsapp.com:443. No hay ningún proxy configurado.")
        }
        "Connected directly to web.whatsapp.com:443. The desktop has no system proxy configured." => {
            Some(
                "Conexión directa a web.whatsapp.com:443. El escritorio no tiene un proxy del sistema configurado.",
            )
        }
        "Connecting…" => Some("Conectando…"),
        "Connection" => Some("Conexión"),
        "Connection mode" => Some("Modo de conexión"),
        "Connection notifications" => Some("Notificaciones de conexión"),
        "Connection notifications also follow Do not disturb and per-account mute." => Some(
            "Las notificaciones de conexión también respetan No molestar y el silencio por cuenta.",
        ),
        "Connection notifications: {state}" => Some("Notificaciones de conexión: {state}"),
        "Connection restored" => Some("Conexión restaurada"),
        "Contrast, text size, and motion preferences for the native shell. They are saved with your settings and included in exports." => {
            Some(
                "Preferencias de contraste, tamaño del texto y movimiento para la carcasa nativa. Se guardan con tu configuración y se incluyen en las exportaciones.",
            )
        }
        "Copy" => Some("Copiar"),
        "Correct while I type" => Some("Corregir mientras escribo"),
        "Could not authenticate with the SOCKS proxy: {error}" => {
            Some("No se pudo autenticar con el proxy SOCKS: {error}")
        }
        "Could not back up the current settings, so nothing was imported: {error}" => Some(
            "No se pudo hacer una copia de la configuración actual, así que no se importó nada: {error}",
        ),
        "Could not back up the current settings, so nothing was reset: {error}" => Some(
            "No se pudo hacer una copia de la configuración actual, así que no se restableció nada: {error}",
        ),
        "Could not export settings: {error}" => {
            Some("No se pudo exportar la configuración: {error}")
        }
        "Could not import settings: {error}" => {
            Some("No se pudo importar la configuración: {error}")
        }
        "Could not inspect {path}: {error}" => Some("No se pudo inspeccionar {path}: {error}"),
        "Could not open the account window" => Some("No se pudo abrir la ventana de la cuenta"),
        "Could not reach {label} at {host}:{port}: {error}" => {
            Some("No se pudo alcanzar {label} en {host}:{port}: {error}")
        }
        "Could not read the SOCKS address: {error}" => {
            Some("No se pudo leer la dirección SOCKS: {error}")
        }
        "Could not read the SOCKS reply: {error}" => {
            Some("No se pudo leer la respuesta SOCKS: {error}")
        }
        "Could not read the desktop proxy settings: {error}" => {
            Some("No se pudo leer la configuración del proxy del escritorio: {error}")
        }
        "Could not read {path}: {error}" => Some("No se pudo leer {path}: {error}"),
        "Could not reconnect WhatsApp Web" => Some("No se pudo reconectar WhatsApp Web"),
        "Could not recover WhatsApp Web" => Some("No se pudo recuperar WhatsApp Web"),
        "Could not resolve {label} ({host}): {error}" => {
            Some("No se pudo resolver {label} ({host}): {error}")
        }
        "Could not send the CONNECT request to the proxy: {error}" => {
            Some("No se pudo enviar la solicitud CONNECT al proxy: {error}")
        }
        "Could not send the SOCKS CONNECT request: {error}" => {
            Some("No se pudo enviar la solicitud CONNECT de SOCKS: {error}")
        }
        "Could not start the SOCKS negotiation: {error}" => {
            Some("No se pudo iniciar la negociación SOCKS: {error}")
        }
        "Could not write {path}: {error}" => Some("No se pudo escribir {path}: {error}"),
        "Could not {operation}: {error}" => Some("No se pudo {operation}: {error}"),
        "Creating a fresh WebKit view. Your session is kept and the page will reconnect automatically." => {
            Some(
                "Creando una vista de WebKit nueva. Tu sesión se conserva y la página se reconectará automáticamente.",
            )
        }
        "Custom color" => Some("Color personalizado"),
        "Dark" => Some("Oscuro"),
        "Data folder: {path}" => Some("Carpeta de datos: {path}"),
        "Data {data} · Cache {cache} · Total {total}{partial}" => {
            Some("Datos {data} · Caché {cache} · Total {total}{partial}")
        }
        "Delete account data" => Some("Eliminar datos de la cuenta"),
        "Delete all data for “{name}”?" => Some("¿Eliminar todos los datos de «{name}»?"),
        "Deleted the account data and removed the account ({freed} freed)." => {
            Some("Se eliminaron los datos y la cuenta ({freed} liberados).")
        }
        "Deleting account data requires typing the account name and removes the account from OxideZap. The last account cannot be deleted." => {
            Some(
                "Eliminar los datos de una cuenta requiere escribir su nombre y la quita de OxideZap. La última cuenta no se puede eliminar.",
            )
        }
        "Desktop notifications" => Some("Notificaciones de escritorio"),
        "Desktop notifications: {state}" => Some("Notificaciones de escritorio: {state}"),
        "Destroy the WebKit view of an account that stayed hidden for the delay below. The account and its WhatsApp session stay on disk." => {
            Some(
                "Destruye la vista de WebKit de una cuenta que permaneció oculta durante el tiempo indicado abajo. La cuenta y su sesión de WhatsApp permanecen en disco.",
            )
        }
        "Development build" => Some("Compilación de desarrollo"),
        "Development builds do not check for updates." => {
            Some("Las compilaciones de desarrollo no buscan actualizaciones.")
        }
        "Disabled" => Some("Desactivado"),
        "Display" => Some("Pantalla"),
        "Display size" => Some("Tamaño de pantalla"),
        "Do not disturb" => Some("No molestar"),
        "Do not disturb: {state}" => Some("No molestar: {state}"),
        "Do not disturb: {status}" => Some("No molestar: {status}"),
        "Download cancelled" => Some("Descarga cancelada"),
        "Download complete" => Some("Descarga completada"),
        "Download could not be retried" => Some("No se pudo reintentar la descarga"),
        "Download could not start" => Some("No se pudo iniciar la descarga"),
        "Download failed" => Some("Descarga fallida"),
        "Download folder" => Some("Carpeta de descargas"),
        "Download in progress" => Some("Descarga en curso"),
        "Downloading" => Some("Descargando"),
        "Downloads" => Some("Descargas"),
        "Downloads appear here as they start." => {
            Some("Las descargas aparecen aquí cuando comienzan.")
        }
        "Downloads are written straight to disk. In-progress rows can be cancelled; finished rows can be retried when WebKit exposed a source URL." => {
            Some(
                "Las descargas se escriben directamente en disco. Las filas en curso se pueden cancelar; las terminadas se pueden reintentar cuando WebKit expuso una URL de origen.",
            )
        }
        "Downloads…" => Some("Descargas…"),
        "Enable the loaded stylesheet on every open WhatsApp Web account." => {
            Some("Activa la hoja de estilos cargada en cada cuenta de WhatsApp Web abierta.")
        }
        "End" => Some("Fin"),
        "English" => Some("Inglés"),
        "Enter a proxy username when a password is set." => {
            Some("Introduce un usuario del proxy cuando hay una contraseña configurada.")
        }
        "Enter a valid IPv6 address in brackets, for example [::1]." => {
            Some("Introduce una dirección IPv6 válida entre corchetes, por ejemplo [::1].")
        }
        "Enter an HTTP, HTTPS, or SOCKS proxy" => Some("Introduce un proxy HTTP, HTTPS o SOCKS"),
        "Enter an absolute folder path, or use the default." => {
            Some("Introduce una ruta de carpeta absoluta o usa la predeterminada.")
        }
        "Enter both quiet-hours times as HH:MM." => {
            Some("Introduce ambas horas del silencio como HH:MM.")
        }
        "Enter the proxy host." => Some("Introduce el host del proxy."),
        "Enter the proxy port." => Some("Introduce el puerto del proxy."),
        "Enter your password to return to WhatsApp. Account windows stay hidden until you unlock." => {
            Some(
                "Introduce tu contraseña para volver a WhatsApp. Las ventanas de cuenta permanecen ocultas hasta que desbloquees.",
            )
        }
        "Enter your password." => Some("Introduce tu contraseña."),
        "Estimated memory released" => Some("Memoria liberada estimada"),
        "Every account gets its own window and its own WebKit profile, so two WhatsApp accounts never share cookies, local storage, or caches." => {
            Some(
                "Cada cuenta tiene su propia ventana y su propio perfil de WebKit, así que dos cuentas de WhatsApp nunca comparten cookies, almacenamiento local ni cachés.",
            )
        }
        "Every account has its own isolated WhatsApp Web session and window." => {
            Some("Cada cuenta tiene su propia sesión de WhatsApp Web aislada y su propia ventana.")
        }
        "Every day" => Some("Todos los días"),
        "Experimental CSS" => Some("CSS experimental"),
        "Experimental CSS disabled." => Some("CSS experimental desactivado."),
        "Experimental CSS enabled." => Some("CSS experimental activado."),
        "Experimental CSS removed. WhatsApp Web uses its own styles again." => {
            Some("CSS experimental eliminado. WhatsApp Web vuelve a usar sus propios estilos.")
        }
        "Experimental CSS: {state} (custom CSS is never imported)" => {
            Some("CSS experimental: {state} (el CSS personalizado nunca se importa)")
        }
        "Export" => Some("Exportar"),
        "Export OxideZap settings" => Some("Exportar configuración de OxideZap"),
        "Export settings…" => Some("Exportar configuración…"),
        "Export the configuration to a readable JSON file, import it on another installation, or restore the defaults." => {
            Some(
                "Exporta la configuración a un archivo JSON legible, impórtala en otra instalación o restaura los valores predeterminados.",
            )
        }
        "Exports never include account sessions, cookies, chats, or the app-lock password. Every import and reset writes a dated backup first." => {
            Some(
                "Las exportaciones nunca incluyen sesiones de cuenta, cookies, chats ni la contraseña del bloqueo. Cada importación y restablecimiento escribe antes una copia de seguridad con fecha.",
            )
        }
        "FIRST-RUN GUIDE" => Some("GUÍA INICIAL"),
        "Failed" => Some("Fallida"),
        "File" => Some("Archivo"),
        "Files saved from WhatsApp Web show their progress and history here. Blob and view-once media never reach this list." => {
            Some(
                "Los archivos guardados desde WhatsApp Web muestran aquí su progreso e historial. Los medios blob y de ver una vez nunca llegan a esta lista.",
            )
        }
        "First-run guide" => Some("Guía inicial"),
        "Folder" => Some("Carpeta"),
        "Follow quiet hours" => Some("Seguir el horario de silencio"),
        "Follow the desktop" => Some("Seguir el escritorio"),
        "Follow the system theme" => Some("Seguir el tema del sistema"),
        "For 1 hour" => Some("Durante 1 hora"),
        "Free disk space" => Some("Espacio libre en disco"),
        "GPUI 0.2.2 has no AccessKit/AT-SPI bridge, so screen readers cannot inspect these native controls. Keyboard focus and the visible focus ring still work, and WhatsApp Web keeps WebKit's own accessibility tree." => {
            Some(
                "GPUI 0.2.2 no tiene un puente AccessKit/AT-SPI, así que los lectores de pantalla no pueden inspeccionar estos controles nativos. El foco de teclado y el anillo de foco visible siguen funcionando, y WhatsApp Web conserva el árbol de accesibilidad propio de WebKit.",
            )
        }
        "GPUI for the native shell · WebKitGTK for WhatsApp Web · Rust for everything in between." => {
            Some(
                "GPUI para la carcasa nativa · WebKitGTK para WhatsApp Web · Rust para todo lo demás.",
            )
        }
        "General" => Some("General"),
        "GitHub repository" => Some("Repositorio de GitHub"),
        "Global appearance: {appearance}" => Some("Apariencia global: {appearance}"),
        "Global shortcut" => Some("Atajo global"),
        "Global shortcut: {from} -> {to}" => Some("Atajo global: {from} -> {to}"),
        "Global shortcuts" => Some("Atajos globales"),
        "Global shortcuts require an X11 or XWayland session." => {
            Some("Los atajos globales requieren una sesión X11 o XWayland.")
        }
        "HH:MM, local time" => Some("HH:MM, hora local"),
        "HH:MM, may cross midnight" => Some("HH:MM, puede cruzar la medianoche"),
        "Hide OxideZap" => Some("Ocultar OxideZap"),
        "Hide all account windows" => Some("Ocultar todas las ventanas de cuenta"),
        "Hide every account window behind a lock screen." => {
            Some("Oculta todas las ventanas de cuenta tras una pantalla de bloqueo.")
        }
        "Hide or restore every window" => Some("Ocultar o restaurar todas las ventanas"),
        "High contrast" => Some("Alto contraste"),
        "High contrast: {state}" => Some("Alto contraste: {state}"),
        "Host" => Some("Host"),
        "Image copied" => Some("Imagen copiada"),
        "Import" => Some("Importar"),
        "Import OxideZap settings" => Some("Importar configuración de OxideZap"),
        "Import settings…" => Some("Importar configuración…"),
        "Import summary" => Some("Resumen de la importación"),
        "In progress" => Some("En curso"),
        "Incorrect password." => Some("Contraseña incorrecta."),
        "Indefinitely" => Some("Indefinidamente"),
        "Inspect and clean the WebKit profile behind each account." => {
            Some("Inspecciona y limpia el perfil de WebKit de cada cuenta.")
        }
        "Interface language" => Some("Idioma de la interfaz"),
        "Interface language: {from} -> {to}" => Some("Idioma de la interfaz: {from} -> {to}"),
        "It can take a few seconds. If the page stays blank, check that WebKitGTK 2.46.1 or newer is installed; older versions can hang the QR login. OxideZap never waits for the QR before showing this guide." => {
            Some(
                "Puede tardar unos segundos. Si la página sigue en blanco, comprueba que WebKitGTK 2.46.1 o posterior esté instalado; las versiones anteriores pueden colgar el inicio de sesión por QR. OxideZap nunca espera al QR antes de mostrar esta guía.",
            )
        }
        "Just now" => Some("Ahora mismo"),
        "Keep the active account loaded" => Some("Mantener cargada la cuenta activa"),
        "Last cleanup" => Some("Última limpieza"),
        "Launch at login" => Some("Iniciar al iniciar sesión"),
        "Launch at login: {state}" => Some("Iniciar al iniciar sesión: {state}"),
        "Leave the default to follow the desktop Downloads directory." => {
            Some("Deja el valor predeterminado para seguir la carpeta de Descargas del escritorio.")
        }
        "Light" => Some("Claro"),
        "Link your phone" => Some("Vincula tu teléfono"),
        "Load" => Some("Cargar"),
        "Load CSS file…" => Some("Cargar archivo CSS…"),
        "Load experimental WhatsApp CSS" => Some("Cargar CSS experimental de WhatsApp"),
        "Loading WhatsApp Web…" => Some("Cargando WhatsApp Web…"),
        "Lock" => Some("Bloquear"),
        "Lock OxideZap" => Some("Bloquear OxideZap"),
        "Lock after idle time" => Some("Bloquear tras inactividad"),
        "Lock automatically when the window is hidden to the tray." => {
            Some("Bloquea automáticamente cuando la ventana se oculta en la bandeja.")
        }
        "Lock before sleeping and after waking, using the logind D-Bus signal when available." => {
            Some(
                "Bloquea antes de suspender y al despertar, usando la señal D-Bus de logind cuando esté disponible.",
            )
        }
        "Lock now" => Some("Bloquear ahora"),
        "Lock on launch" => Some("Bloquear al iniciar"),
        "Lock on system suspend" => Some("Bloquear al suspender el sistema"),
        "Lock when hidden" => Some("Bloquear al ocultar"),
        "Lock when the desktop screen locks" => Some("Bloquear cuando se bloquee la pantalla"),
        "Log in to a second number from Settings > Accounts > Add account. Removing an account also removes its local profile." => {
            Some(
                "Inicia sesión con un segundo número desde Ajustes > Cuentas > Añadir cuenta. Quitar una cuenta también elimina su perfil local.",
            )
        }
        "Low-light surfaces" => Some("Superficies oscuras"),
        "Low-memory mode" => Some("Modo de bajo consumo de memoria"),
        "Low-memory mode: {state}" => Some("Modo de bajo consumo de memoria: {state}"),
        "MIT licensed. The source code is available on GitHub." => {
            Some("Licencia MIT. El código fuente está disponible en GitHub.")
        }
        "Manual proxy" => Some("Proxy manual"),
        "Maximize" => Some("Maximizar"),
        "Media action failed" => Some("La acción de medios falló"),
        "Media action menu" => Some("Menú de acciones de medios"),
        "Media action menu: {state}" => Some("Menú de acciones de medios: {state}"),
        "Message previews use your desktop's notification service. Toggle them globally, mute one account, or turn on Do Not Disturb in Settings > Notifications." => {
            Some(
                "Las vistas previas de mensajes usan el servicio de notificaciones del escritorio. Actívalas globalmente, silencia una cuenta o activa No molestar en Ajustes > Notificaciones.",
            )
        }
        "Minimize" => Some("Minimizar"),
        "Mode: {mode} · Status: {status}" => Some("Modo: {mode} · Estado: {status}"),
        "Mute notifications" => Some("Silenciar notificaciones"),
        "Muted" => Some("Silenciada"),
        "Muted accounts and Do not disturb have priority over per-account sounds." => Some(
            "Las cuentas silenciadas y No molestar tienen prioridad sobre los sonidos por cuenta.",
        ),
        "Native notifications" => Some("Notificaciones nativas"),
        "Network" => Some("Red"),
        "Network proxy credentials: removed" => Some("Credenciales del proxy de red: eliminadas"),
        "Network proxy: {from} -> {to}" => Some("Proxy de red: {from} -> {to}"),
        "Never" => Some("Nunca"),
        "Never release the account you opened most recently, even when every window is hidden." => {
            Some(
                "Nunca liberes la cuenta que abriste más recientemente, incluso cuando todas las ventanas estén ocultas.",
            )
        }
        "New password" => Some("Contraseña nueva"),
        "Next" => Some("Siguiente"),
        "No downloads yet" => Some("Aún no hay descargas"),
        "No hidden account could be released right now. Downloads, calls or recordings, the active account, the app lock, and quitting all block suspension." => {
            Some(
                "No se pudo liberar ninguna cuenta oculta ahora mismo. Las descargas, llamadas o grabaciones, la cuenta activa, el bloqueo de la aplicación y el cierre bloquean la suspensión.",
            )
        }
        "No matching commands" => Some("No hay comandos coincidentes"),
        "No preference changes." => Some("No hay cambios de preferencias."),
        "No proxy" => Some("Sin proxy"),
        "No stylesheet loaded." => Some("No hay ninguna hoja de estilos cargada."),
        "No telemetry" => Some("Sin telemetría"),
        "No unread messages" => Some("No hay mensajes sin leer"),
        "Not completed. It opens automatically only on a new installation." => {
            Some("No completada. Solo se abre automáticamente en una instalación nueva.")
        }
        "Not scanned yet" => Some("Aún no escaneado"),
        "Notification sound" => Some("Sonido de notificación"),
        "Notifications" => Some("Notificaciones"),
        "Notifications and sound" => Some("Notificaciones y sonido"),
        "Notifications, silent" => Some("Notificaciones, silenciosas"),
        "Off" => Some("Desactivado"),
        "Offline" => Some("Desconectado"),
        "On" => Some("Activado"),
        "On for 1 hour" => Some("Durante 1 hora"),
        "On until you turn it off" => Some("Activado hasta que lo desactives"),
        "On until {point} ({remaining})" => Some("Activado hasta {point} ({remaining})"),
        "Open" => Some("Abrir"),
        "Open Download Center" => Some("Abrir el centro de descargas"),
        "Open WhatsApp Web" => Some("Abrir WhatsApp Web"),
        "Open WhatsApp on your phone." => Some("Abre WhatsApp en tu teléfono."),
        "Open data folder" => Some("Abrir carpeta de datos"),
        "Open download center" => Some("Abrir el centro de descargas"),
        "Open download folder" => Some("Abrir carpeta de descargas"),
        "Open downloads folder" => Some("Abrir carpeta de descargas"),
        "Open folder" => Some("Abrir carpeta"),
        "Open settings" => Some("Abrir ajustes"),
        "Open storage folder" => Some("Abrir carpeta de almacenamiento"),
        "Open the Download Center to retry." => {
            Some("Abre el centro de descargas para reintentar.")
        }
        "Opens the command palette while any application is focused. Leave empty to use Ctrl+K only inside the native window." => {
            Some(
                "Abre la paleta de comandos mientras cualquier aplicación tiene el foco. Déjalo vacío para usar Ctrl+K solo dentro de la ventana nativa.",
            )
        }
        "Optional" => Some("Opcional"),
        "Optional global shortcut" => Some("Atajo global opcional"),
        "Optional password" => Some("Contraseña opcional"),
        "Optional username" => Some("Usuario opcional"),
        "Optional, e.g. Ctrl+Shift+K" => Some("Opcional, p. ej. Ctrl+Shift+K"),
        "Optional, stored as plain text" => Some("Opcional, guardada como texto plano"),
        "Organize by account" => Some("Organizar por cuenta"),
        "Other accounts will use the same theme and zoom overrides. Their sessions and window bounds are kept." => {
            Some(
                "Las demás cuentas usarán el mismo tema y los mismos ajustes de zoom. Sus sesiones y límites de ventana se conservan.",
            )
        }
        "Organize by month" => Some("Organizar por mes"),
        "OxideZap Downloads" => Some("Descargas de OxideZap"),
        "OxideZap Locked" => Some("OxideZap bloqueado"),
        "OxideZap Settings" => Some("Ajustes de OxideZap"),
        "OxideZap command palette" => Some("Paleta de comandos de OxideZap"),
        "OxideZap creates its files with private 0600/0700 permissions. The download folder stays shared with your desktop." => {
            Some(
                "OxideZap crea sus archivos con permisos privados 0600/0700. La carpeta de descargas sigue compartida con tu escritorio.",
            )
        }
        "OxideZap did not reload WhatsApp Web while a download is active." => {
            Some("OxideZap no recargó WhatsApp Web mientras hay una descarga activa.")
        }
        "OxideZap did not reload WhatsApp Web while a download is active. Retry when the download finishes." => {
            Some(
                "OxideZap no recargó WhatsApp Web mientras hay una descarga activa. Reinténtalo cuando termine la descarga.",
            )
        }
        "OxideZap is an unofficial, independent project. It is not affiliated with, endorsed by, or sponsored by WhatsApp or Meta. WhatsApp is a trademark of Meta Platforms, Inc." => {
            Some(
                "OxideZap es un proyecto independiente y no oficial. No está afiliado, respaldado ni patrocinado por WhatsApp o Meta. WhatsApp es una marca de Meta Platforms, Inc.",
            )
        }
        "OxideZap is locked" => Some("OxideZap está bloqueado"),
        "OxideZap is trying to recover the page." => {
            Some("OxideZap está intentando recuperar la página.")
        }
        "OxideZap settings (*.json)" => Some("Configuración de OxideZap (*.json)"),
        "OxideZap stopped automatic reconnects for {name} after {attempts} attempts. Use Retry now in the account window." => {
            Some(
                "OxideZap detuvo las reconexiones automáticas de {name} tras {attempts} intentos. Usa Reintentar ahora en la ventana de la cuenta.",
            )
        }
        "OxideZap will reconnect when the network returns." => {
            Some("OxideZap se reconectará cuando vuelva la red.")
        }
        "OxideZap wraps the official WhatsApp Web in a native Linux window. It is an independent, unofficial project, not affiliated with or endorsed by WhatsApp or Meta." => {
            Some(
                "OxideZap envuelve WhatsApp Web oficial en una ventana nativa de Linux. Es un proyecto independiente y no oficial, no afiliado ni respaldado por WhatsApp o Meta.",
            )
        }
        "OxideZap — {count} unread message" => Some("OxideZap — {count} mensaje sin leer"),
        "OxideZap — {count} unread messages" => Some("OxideZap — {count} mensajes sin leer"),
        "Password" => Some("Contraseña"),
        "Pause CSS animations and transitions in WhatsApp Web. The native shell has no animated transitions today; any future native transition must honor this setting too. It works independently of battery saver." => {
            Some(
                "Pausa las animaciones y transiciones CSS de WhatsApp Web. La carcasa nativa no tiene transiciones animadas hoy; cualquier transición nativa futura también deberá respetar esta opción. Funciona independientemente del ahorro de batería.",
            )
        }
        "Pause native notifications and sounds from every account. Unread counts, window titles, and the tray badge keep updating." => {
            Some(
                "Pausa las notificaciones nativas y los sonidos de todas las cuentas. Los contadores sin leer, los títulos de ventana y la insignia de la bandeja siguen actualizándose.",
            )
        }
        "Per-account" => Some("Por cuenta"),
        "Performance" => Some("Rendimiento"),
        "Performance mode: {from} -> {to}" => Some("Modo de rendimiento: {from} -> {to}"),
        "Point the phone at the QR code in the OxideZap window." => {
            Some("Apunta el teléfono al código QR de la ventana de OxideZap.")
        }
        "Port" => Some("Puerto"),
        "Prefer the full walkthrough? The README documents the same steps." => {
            Some("¿Prefieres el recorrido completo? El README documenta los mismos pasos.")
        }
        "Preferences reset to their defaults. Your accounts were kept and a backup was written to {path}." => {
            Some(
                "Preferencias restablecidas a sus valores predeterminados. Tus cuentas se conservaron y se escribió una copia de seguridad en {path}.",
            )
        }
        "Preview text" => Some("Texto de vista previa"),
        "Progress" => Some("Progreso"),
        "Proxy credentials are stored as plain text in settings.json. The file is private (0600) and credentials are never logged or written to exports, but this build does not use a system keyring. Prefer a proxy that does not require a password." => {
            Some(
                "Las credenciales del proxy se guardan como texto plano en settings.json. El archivo es privado (0600) y las credenciales nunca se registran ni se escriben en exportaciones, pero esta compilación no usa un llavero del sistema. Prefiere un proxy que no requiera contraseña.",
            )
        }
        "Proxy protocol" => Some("Protocolo del proxy"),
        "Proxy settings saved. Reopen an account if the new route does not take effect." => {
            Some("Ajustes del proxy guardados. Reabre una cuenta si la nueva ruta no surte efecto.")
        }
        "QR code does not load?" => Some("¿No se carga el código QR?"),
        "Quiet hours" => Some("Horario de silencio"),
        "Quiet hours from {point}" => Some("Silencio desde {point}"),
        "Quiet hours have no active days" => Some("El silencio no tiene días activos"),
        "Quiet hours on" => Some("Silencio activado"),
        "Quiet hours skipped until {point}" => Some("Silencio omitido hasta {point}"),
        "Quiet hours until {point} ({remaining})" => Some("Silencio hasta {point} ({remaining})"),
        "Quiet hours use the desktop's local time. Overnight ranges such as 22:00-07:00 are supported; each period belongs to the day it starts. Times are re-evaluated when the clock changes or the machine resumes, and daylight-saving transitions follow the desktop timezone." => {
            Some(
                "El horario de silencio usa la hora local del escritorio. Se admiten rangos nocturnos como 22:00-07:00; cada periodo pertenece al día en que comienza. Las horas se reevalúan cuando cambia el reloj o el equipo se reanuda, y los cambios de horario de verano siguen la zona horaria del escritorio.",
            )
        }
        "Quit" => Some("Salir"),
        "Quit OxideZap" => Some("Salir de OxideZap"),
        "Reactivate" => Some("Reactivar"),
        "Reactivating reloads WhatsApp Web and can take a few seconds. The current scroll position or an unsent draft may be lost." => {
            Some(
                "Reactivar recarga WhatsApp Web y puede tardar unos segundos. La posición de desplazamiento actual o un borrador sin enviar podrían perderse.",
            )
        }
        "Reactivating reloads WhatsApp Web, which takes a few seconds. The current scroll position or an unsent draft may be lost." => {
            Some(
                "Reactivar recarga WhatsApp Web, lo que tarda unos segundos. La posición de desplazamiento actual o un borrador sin enviar podrían perderse.",
            )
        }
        "Read the guide" => Some("Leer la guía"),
        "Reconnecting…" => Some("Reconectando…"),
        "Recurring window: {window}." => Some("Ventana recurrente: {window}."),
        "Reduce animation and polling work without turning off notifications, downloads, calls, the app lock, or security checks." => {
            Some(
                "Reduce el trabajo de animación y sondeo sin desactivar notificaciones, descargas, llamadas, el bloqueo de la aplicación ni las comprobaciones de seguridad.",
            )
        }
        "Reduce motion" => Some("Reducir movimiento"),
        "Reduce motion is independent of battery saver and stays active when the saver is off or the device is on AC power." => {
            Some(
                "Reducir movimiento es independiente del ahorro de batería y sigue activo cuando el ahorro está desactivado o el dispositivo usa corriente alterna.",
            )
        }
        "Reduce motion: {state}" => Some("Reducir movimiento: {state}"),
        "Refresh" => Some("Actualizar"),
        "Release hidden accounts after" => Some("Liberar cuentas ocultas tras"),
        "Release hidden accounts now" => Some("Liberar cuentas ocultas ahora"),
        "Released {count} hidden account. Open the account to restore it." => {
            Some("Se liberó {count} cuenta oculta. Abre la cuenta para restaurarla.")
        }
        "Released {count} hidden accounts. Open the account to restore it." => {
            Some("Se liberaron {count} cuentas ocultas. Abre la cuenta para restaurarla.")
        }
        "Remember window bounds: {state}" => Some("Recordar límites de ventana: {state}"),
        "Remember window size and position" => Some("Recordar tamaño y posición de la ventana"),
        "Remove" => Some("Quitar"),
        "Remove account" => Some("Quitar cuenta"),
        "Remove password" => Some("Quitar contraseña"),
        "Remove password protection?" => Some("¿Quitar la protección con contraseña?"),
        "Remove this account?" => Some("¿Quitar esta cuenta?"),
        "Rename" => Some("Renombrar"),
        "Reopen the media and use the save action to retry." => {
            Some("Vuelve a abrir el medio y usa la acción de guardar para reintentar.")
        }
        "Report an issue" => Some("Informar de un problema"),
        "Require a password to unlock" => Some("Requerir contraseña para desbloquear"),
        "Reset" => Some("Restablecer"),
        "Reset identity" => Some("Restablecer identidad"),
        "Reset preferences to their defaults?" => {
            Some("¿Restablecer las preferencias a sus valores predeterminados?")
        }
        "Reset settings" => Some("Restablecer ajustes"),
        "Reset zoom" => Some("Restablecer zoom"),
        "Restore each account window where you left it." => {
            Some("Restaura cada ventana de cuenta donde la dejaste.")
        }
        "Restoring WhatsApp Web…" => Some("Restaurando WhatsApp Web…"),
        "Restricted to https://web.whatsapp.com · 64 KiB limit · CSS only · excluded from settings exports." => {
            Some(
                "Limitado a https://web.whatsapp.com · límite de 64 KiB · solo CSS · excluido de las exportaciones de configuración.",
            )
        }
        "Resume all suspended accounts" => Some("Reanudar todas las cuentas suspendidas"),
        "Retry" => Some("Reintentar"),
        "Retry connection" => Some("Reintentar conexión"),
        "Retry now" => Some("Reintentar ahora"),
        "Retry unavailable" => Some("Reintento no disponible"),
        "Review the WhatsApp Web login, the QR steps, account isolation, tray and close behavior, notifications, the app lock, and where OxideZap stores data." => {
            Some(
                "Repasa el inicio de sesión de WhatsApp Web, los pasos del QR, el aislamiento de cuentas, el comportamiento de la bandeja y el cierre, las notificaciones, el bloqueo de la aplicación y dónde guarda los datos OxideZap.",
            )
        }
        "Right-click normal media to save it, open it, or copy an image. View-once and protected media are never offered." => {
            Some(
                "Haz clic derecho en medios normales para guardarlos, abrirlos o copiar una imagen. Los medios de ver una vez y protegidos nunca se ofrecen.",
            )
        }
        "Running storage operation…" => Some("Ejecutando operación de almacenamiento…"),
        "Rust · GPUI · WebKitGTK" => Some("Rust · GPUI · WebKitGTK"),
        "Save" => Some("Guardar"),
        "Save each account's files in its own subfolder." => {
            Some("Guarda los archivos de cada cuenta en su propia subcarpeta.")
        }
        "Saved configuration: {summary}" => Some("Configuración guardada: {summary}"),
        "Saved to {path}" => Some("Guardado en {path}"),
        "Saving to" => Some("Guardando en"),
        "Scanning…" => Some("Escaneando…"),
        "Scheduled" => Some("Programado"),
        "Search commands…" => Some("Buscar comandos…"),
        "Security" => Some("Seguridad"),
        "Select" => Some("Seleccionar"),
        "Selected · {description}" => Some("Seleccionado · {description}"),
        "Set a password" => Some("Configurar una contraseña"),
        "Set a password in Settings > Security to hide every window behind a privacy screen: lock now, on launch, on hide, on suspend, when the desktop locks, or after an idle timeout." => {
            Some(
                "Configura una contraseña en Ajustes > Seguridad para ocultar todas las ventanas tras una pantalla de privacidad: bloquea ahora, al iniciar, al ocultar, al suspender, cuando se bloquee el escritorio o tras un tiempo de inactividad.",
            )
        }
        "Set password" => Some("Configurar contraseña"),
        "Settings" => Some("Ajustes"),
        "Settings — {appearance}" => Some("Ajustes — {appearance}"),
        "Settings exported to {path}." => Some("Configuración exportada a {path}."),
        "Settings imported. The previous settings were backed up to {path}." => {
            Some("Configuración importada. La configuración anterior se guardó en {path}.")
        }
        "Settings…" => Some("Ajustes…"),
        "Show OxideZap" => Some("Mostrar OxideZap"),
        "Show a native notification when a message arrives while OxideZap is in the background." => {
            Some(
                "Muestra una notificación nativa cuando llega un mensaje mientras OxideZap está en segundo plano.",
            )
        }
        "Show a native notification when an account goes offline or reconnects." => {
            Some("Muestra una notificación nativa cuando una cuenta se desconecta o se reconecta.")
        }
        "Show all account windows" => Some("Mostrar todas las ventanas de cuenta"),
        "Show or hide OxideZap" => Some("Mostrar u ocultar OxideZap"),
        "Show the first-run guide again" => Some("Mostrar la guía inicial de nuevo"),
        "Sign out" => Some("Cerrar sesión"),
        "Sign out of “{name}”?" => Some("¿Cerrar sesión en «{name}»?"),
        "Signed out. The account was kept; open it and scan the QR code again." => {
            Some("Sesión cerrada. La cuenta se conservó; ábrela y escanea el código QR de nuevo.")
        }
        "Single-phone account" => Some("Cuenta de un solo teléfono"),
        "Size" => Some("Tamaño"),
        "Skip" => Some("Omitir"),
        "Skip this quiet period" => Some("Omitir este periodo de silencio"),
        "Skip this version" => Some("Omitir esta versión"),
        "Skipping hides this guide permanently." => {
            Some("Omitirla oculta esta guía de forma permanente.")
        }
        "Sound playback depends on the notification service and its sound theme; some servers ignore the requested sound or always use their own." => {
            Some(
                "La reproducción de sonido depende del servicio de notificaciones y de su tema de sonidos; algunos servidores ignoran el sonido solicitado o siempre usan el suyo.",
            )
        }
        "Spanish" => Some("Español"),
        "Spell check language" => Some("Idioma de corrección ortográfica"),
        "Start" => Some("Inicio"),
        "Start OxideZap automatically when you sign in to the desktop." => {
            Some("Inicia OxideZap automáticamente al iniciar sesión en el escritorio.")
        }
        "Started" => Some("Inicio"),
        "Status" => Some("Estado"),
        "Step {step} of {total}" => Some("Paso {step} de {total}"),
        "Storage" => Some("Almacenamiento"),
        "Suppress native message notifications from this account." => {
            Some("Suprime las notificaciones nativas de mensajes de esta cuenta.")
        }
        "Suspended accounts" => Some("Cuentas suspendidas"),
        "Suspending never deletes the account, its WebKit profile, cookies, local storage, or the WhatsApp login. It only destroys the running view, which lets WebKit terminate that account's web process. Downloads, calls, and recordings always block suspension, and nothing is released while the app is locked or quitting." => {
            Some(
                "Suspender nunca elimina la cuenta, su perfil de WebKit, las cookies, el almacenamiento local ni el inicio de sesión de WhatsApp. Solo destruye la vista en ejecución, lo que permite que WebKit termine el proceso web de esa cuenta. Las descargas, llamadas y grabaciones siempre bloquean la suspensión, y no se libera nada mientras la aplicación está bloqueada o cerrando.",
            )
        }
        "Switch to account {position}: {name}" => Some("Cambiar a la cuenta {position}: {name}"),
        "Switch to account {position}: {name} (current)" => {
            Some("Cambiar a la cuenta {position}: {name} (actual)")
        }
        "System" => Some("Sistema"),
        "System follows the desktop color scheme live. On Linux this depends on the XDG desktop portal and the desktop settings service, so some window managers may not report changes." => {
            Some(
                "Sistema sigue el esquema de color del escritorio en vivo. En Linux esto depende del portal XDG y del servicio de ajustes del escritorio, por lo que algunos gestores de ventanas podrían no notificar cambios.",
            )
        }
        "System proxy" => Some("Proxy del sistema"),
        "Tap Linked devices, then Link a device." => {
            Some("Toca Dispositivos vinculados y luego Vincular un dispositivo.")
        }
        "Test connection" => Some("Probar conexión"),
        "Test connection checks web.whatsapp.com through the configured route without blocking the interface. HTTP and SOCKS5 proxies are tested end to end; an HTTPS proxy is checked for reachability before the TLS handshake." => {
            Some(
                "Probar conexión comprueba web.whatsapp.com a través de la ruta configurada sin bloquear la interfaz. Los proxies HTTP y SOCKS5 se prueban de extremo a extremo; en un proxy HTTPS se comprueba la accesibilidad antes del handshake TLS.",
            )
        }
        "Testing…" => Some("Probando…"),
        "That is everything. Your settings are saved as you change them, and you can reopen this guide any time from Settings > General." => {
            Some(
                "Eso es todo. Tus ajustes se guardan a medida que los cambias y puedes volver a abrir esta guía en cualquier momento desde Ajustes > General.",
            )
        }
        "The CSS contains a NUL character." => Some("El CSS contiene un carácter NUL."),
        "The CSS file is too large ({bytes} bytes; the limit is {limit} KiB)." => {
            Some("El archivo CSS es demasiado grande ({bytes} bytes; el límite es {limit} KiB).")
        }
        "The CSS is too large ({bytes} bytes; the limit is {limit} KiB)." => {
            Some("El CSS es demasiado grande ({bytes} bytes; el límite es {limit} KiB).")
        }
        "The CSS must not contain a </style> tag." => {
            Some("El CSS no debe contener una etiqueta </style>.")
        }
        "The CSS must not contain javascript: URLs." => {
            Some("El CSS no debe contener URL javascript:.")
        }
        "The CSS must not load remote resources. Use data: URIs or local rules instead." => {
            Some("El CSS no debe cargar recursos remotos. Usa URI data: o reglas locales.")
        }
        "The CSS must not use @import. Paste the rules directly instead." => {
            Some("El CSS no debe usar @import. Pega las reglas directamente.")
        }
        "The HTTP proxy at {endpoint} accepted a CONNECT tunnel to web.whatsapp.com:443." => {
            Some("El proxy HTTP en {endpoint} aceptó un túnel CONNECT a web.whatsapp.com:443.")
        }
        "The HTTPS proxy at {endpoint} is reachable. The test stops before the TLS handshake; opening an account verifies the full connection." => {
            Some(
                "El proxy HTTPS en {endpoint} es accesible. La prueba se detiene antes del handshake TLS; abrir una cuenta verifica la conexión completa.",
            )
        }
        "The SOCKS proxy at {endpoint} connected to web.whatsapp.com:443." => {
            Some("El proxy SOCKS en {endpoint} se conectó a web.whatsapp.com:443.")
        }
        "The SOCKS proxy connection timed out." => {
            Some("La conexión al proxy SOCKS agotó el tiempo de espera.")
        }
        "The SOCKS proxy could not reach the network." => {
            Some("El proxy SOCKS no pudo llegar a la red.")
        }
        "The SOCKS proxy could not reach web.whatsapp.com." => {
            Some("El proxy SOCKS no pudo llegar a web.whatsapp.com.")
        }
        "The SOCKS proxy denied the connection." => Some("El proxy SOCKS denegó la conexión."),
        "The SOCKS proxy did not accept username and password authentication." => {
            Some("El proxy SOCKS no aceptó la autenticación con usuario y contraseña.")
        }
        "The SOCKS proxy did not answer CONNECT: {error}" => {
            Some("El proxy SOCKS no respondió a CONNECT: {error}")
        }
        "The SOCKS proxy did not answer the login: {error}" => {
            Some("El proxy SOCKS no respondió al inicio de sesión: {error}")
        }
        "The SOCKS proxy did not answer: {error}" => Some("El proxy SOCKS no respondió: {error}"),
        "The SOCKS proxy does not support CONNECT." => Some("El proxy SOCKS no admite CONNECT."),
        "The SOCKS proxy does not support this address type." => {
            Some("El proxy SOCKS no admite este tipo de dirección.")
        }
        "The SOCKS proxy refused the connection to web.whatsapp.com." => {
            Some("El proxy SOCKS rechazó la conexión a web.whatsapp.com.")
        }
        "The SOCKS proxy rejected the offered authentication methods." => {
            Some("El proxy SOCKS rechazó los métodos de autenticación ofrecidos.")
        }
        "The SOCKS proxy rejected the username or password." => {
            Some("El proxy SOCKS rechazó el usuario o la contraseña.")
        }
        "The SOCKS proxy reported a general failure." => {
            Some("El proxy SOCKS informó de un fallo general.")
        }
        "The SOCKS proxy requires authentication." => {
            Some("El proxy SOCKS requiere autenticación.")
        }
        "The SOCKS proxy returned an unknown address type." => {
            Some("El proxy SOCKS devolvió un tipo de dirección desconocido.")
        }
        "The SOCKS proxy returned an unknown error." => {
            Some("El proxy SOCKS devolvió un error desconocido.")
        }
        "The SOCKS username and password must each be at most 255 bytes." => {
            Some("El usuario y la contraseña de SOCKS deben tener como máximo 255 bytes cada uno.")
        }
        "The account and its settings are kept, but its WebKit data is deleted. WhatsApp Web will ask for the QR code again." => {
            Some(
                "La cuenta y sus ajustes se conservan, pero sus datos de WebKit se eliminan. WhatsApp Web volverá a pedir el código QR.",
            )
        }
        "The account window behind this guide shows WhatsApp Web's QR code. Keep the guide open while you pick up your phone, or skip it and come back from Settings > General later." => {
            Some(
                "La ventana de la cuenta detrás de esta guía muestra el código QR de WhatsApp Web. Mantén la guía abierta mientras coges el teléfono, u omítela y vuelve más tarde desde Ajustes > General.",
            )
        }
        "The app lock uses Argon2id. System suspend and screen-lock detection are best effort through D-Bus." => {
            Some(
                "El bloqueo de la aplicación usa Argon2id. La detección de suspensión del sistema y de bloqueo de pantalla es de mejor esfuerzo mediante D-Bus.",
            )
        }
        "The boss key must differ from the command palette shortcut." => {
            Some("El atajo de jefe debe ser distinto del atajo de la paleta de comandos.")
        }
        "The boss key must differ from the show/hide shortcut." => {
            Some("El atajo de jefe debe ser distinto del atajo de mostrar/ocultar.")
        }
        "The boss key uses the same X11/XWayland grab and can conflict with desktop shortcuts." => {
            Some(
                "El atajo de jefe usa la misma captura X11/XWayland y puede entrar en conflicto con atajos del escritorio.",
            )
        }
        "The command palette shortcut must differ from the boss key." => {
            Some("El atajo de la paleta de comandos debe ser distinto del atajo de jefe.")
        }
        "The command palette shortcut must differ from the show/hide shortcut." => Some(
            "El atajo de la paleta de comandos debe ser distinto del atajo de mostrar/ocultar.",
        ),
        "The default show/hide shortcut is Ctrl+Shift+W. The boss key, Ctrl+Shift+H, hides every window immediately for privacy and restores them on the next press." => {
            Some(
                "El atajo predeterminado de mostrar/ocultar es Ctrl+Shift+W. El atajo de jefe, Ctrl+Shift+H, oculta todas las ventanas de inmediato por privacidad y las restaura al pulsarlo de nuevo.",
            )
        }
        "The desktop proxy settings contain an invalid proxy URI." => {
            Some("La configuración del proxy del escritorio contiene una URI de proxy no válida.")
        }
        "The desktop proxy URI has no host." => {
            Some("La URI del proxy del escritorio no tiene host.")
        }
        "The desktop proxy URI uses port 0." => {
            Some("La URI del proxy del escritorio usa el puerto 0.")
        }
        "The download history keeps the last {count} entries. Existing files are never overwritten; collisions get a numbered suffix." => {
            Some(
                "El historial de descargas conserva las últimas {count} entradas. Los archivos existentes nunca se sobrescriben; las colisiones reciben un sufijo numérico.",
            )
        }
        "The estimate assumes about {size} per released WebKit web process. WebKit does not expose per-view memory, so this is not a measurement." => {
            Some(
                "La estimación supone unos {size} por cada proceso web de WebKit liberado. WebKit no expone la memoria por vista, así que no es una medición.",
            )
        }
        "The file is not a valid OxideZap settings export." => {
            Some("El archivo no es una exportación de configuración de OxideZap válida.")
        }
        "The image is on the clipboard." => Some("La imagen está en el portapapeles."),
        "The local WhatsApp session, cookies, and cached files for this account will be deleted. This cannot be undone." => {
            Some(
                "La sesión local de WhatsApp, las cookies y los archivos en caché de esta cuenta se eliminarán. Esto no se puede deshacer.",
            )
        }
        "The official login" => Some("El inicio de sesión oficial"),
        "The page kept crashing. Your session and messages are safe. Retry when you are ready." => {
            Some(
                "La página seguía fallando. Tu sesión y tus mensajes están a salvo. Reinténtalo cuando quieras.",
            )
        }
        "The password is stored as an Argon2id hash in your config directory." => {
            Some("La contraseña se guarda como un hash Argon2id en tu directorio de configuración.")
        }
        "The passwords do not match." => Some("Las contraseñas no coinciden."),
        "The proxy address is not a valid URL." => {
            Some("La dirección del proxy no es una URL válida.")
        }
        "The proxy did not answer the CONNECT request: {error}" => {
            Some("El proxy no respondió a la solicitud CONNECT: {error}")
        }
        "The proxy did not speak SOCKS5." => Some("El proxy no habla SOCKS5."),
        "The proxy host must be a hostname or IP address, without a scheme, path, or credentials." => {
            Some(
                "El host del proxy debe ser un nombre de host o una dirección IP, sin esquema, ruta ni credenciales.",
            )
        }
        "The proxy is applied to each account's WebKit data manager when the account opens. Existing connections keep their route, so reopen an account if the change does not take effect." => {
            Some(
                "El proxy se aplica al gestor de datos de WebKit de cada cuenta cuando esta se abre. Las conexiones existentes conservan su ruta, así que reabre una cuenta si el cambio no surte efecto.",
            )
        }
        "The proxy port must be a number between 1 and 65535." => {
            Some("El puerto del proxy debe ser un número entre 1 y 65535.")
        }
        "The proxy port must be between 1 and 65535." => {
            Some("El puerto del proxy debe estar entre 1 y 65535.")
        }
        "The proxy rejected the credentials (HTTP 407)." => {
            Some("El proxy rechazó las credenciales (HTTP 407).")
        }
        "The proxy requires a username and password (HTTP 407)." => {
            Some("El proxy requiere un usuario y una contraseña (HTTP 407).")
        }
        "The proxy returned HTTP {status} while opening the tunnel to web.whatsapp.com." => {
            Some("El proxy devolvió HTTP {status} al abrir el túnel a web.whatsapp.com.")
        }
        "The proxy returned an unexpected response to CONNECT." => {
            Some("El proxy devolvió una respuesta inesperada a CONNECT.")
        }
        "The quiet-hours start and end times must differ." => {
            Some("Las horas de inicio y fin del silencio deben ser distintas.")
        }
        "The settings export is too large ({bytes} bytes; the limit is {limit} bytes)." => Some(
            "La exportación de configuración es demasiado grande ({bytes} bytes; el límite es {limit} bytes).",
        ),
        "The show/hide shortcut must differ from the boss key." => {
            Some("El atajo de mostrar/ocultar debe ser distinto del atajo de jefe.")
        }
        "The show/hide shortcut must differ from the command palette shortcut." => Some(
            "El atajo de mostrar/ocultar debe ser distinto del atajo de la paleta de comandos.",
        ),
        "Text files (*.txt)" => Some("Archivos de texto (*.txt)"),
        "Text size" => Some("Tamaño del texto"),
        "Text size: {from}% -> {to}%" => Some("Tamaño del texto: {from}% -> {to}%"),
        "Text size scales the Settings window, the lock screen, and the native text in account windows. Icons, spacing, window chrome, text-field contents, the command palette, and the other native windows keep their fixed sizes; WhatsApp Web zoom is separate." => {
            Some(
                "El tamaño del texto escala la ventana de Ajustes, la pantalla de bloqueo y el texto nativo de las ventanas de cuenta. Los iconos, el espaciado, el marco de la ventana, el contenido de los campos de texto, la paleta de comandos y las demás ventanas nativas mantienen sus tamaños fijos; el zoom de WhatsApp Web es independiente.",
            )
        }
        "Theme" => Some("Tema"),
        "Theme, zoom, notifications, startup, the global shortcut, the app-lock password, and the first-run guide status are reset. Accounts and their WhatsApp sessions are kept, and the first-run guide opens again. A dated backup is written first." => {
            Some(
                "Se restablecen el tema, el zoom, las notificaciones, el inicio, el atajo global, la contraseña del bloqueo y el estado de la guía inicial. Las cuentas y sus sesiones de WhatsApp se conservan, y la guía inicial se abre de nuevo. Antes se escribe una copia de seguridad con fecha.",
            )
        }
        "Theme: {from} -> {to}" => Some("Tema: {from} -> {to}"),
        "This account: {appearance}" => Some("Esta cuenta: {appearance}"),
        "This closes the account, deletes its WebKit profile, and removes it from OxideZap. This cannot be undone." => {
            Some(
                "Esto cierra la cuenta, elimina su perfil de WebKit y la quita de OxideZap. No se puede deshacer.",
            )
        }
        "This download has no reusable source URL. Start it again from WhatsApp Web." => Some(
            "Esta descarga no tiene una URL de origen reutilizable. Iníciala de nuevo desde WhatsApp Web.",
        ),
        "This guide makes no network requests. The app collects no analytics and never asks for your phone number or WhatsApp credentials." => {
            Some(
                "Esta guía no hace solicitudes de red. La aplicación no recopila analíticas y nunca pide tu número de teléfono ni tus credenciales de WhatsApp.",
            )
        }
        "This text is shown at {percent}% of its base size." => {
            Some("Este texto se muestra al {percent}% de su tamaño base.")
        }
        "Total account data" => Some("Datos totales de las cuentas"),
        "Translates the native shell and its windows. WhatsApp Web content keeps its own language." => {
            Some(
                "Traduce la carcasa nativa y sus ventanas. El contenido de WhatsApp Web conserva su propio idioma.",
            )
        }
        "Try a different search." => Some("Prueba con otra búsqueda."),
        "Turn off Do Not Disturb" => Some("Desactivar No molestar"),
        "Turn on Do Not Disturb" => Some("Activar No molestar"),
        "Type the account name exactly to confirm." => {
            Some("Escribe el nombre de la cuenta exactamente para confirmar.")
        }
        "Type the account name to confirm" => Some("Escribe el nombre de la cuenta para confirmar"),
        "Unread count in window title" => Some("Contador sin leer en el título de la ventana"),
        "Prefix the title with the account's unread count, for example (3) OxideZap — Work." => {
            Some(
                "Prefija el título con el contador sin leer de la cuenta, por ejemplo (3) OxideZap — Work.",
            )
        }
        "Unavailable" => Some("No disponible"),
        "Unlock" => Some("Desbloquear"),
        "Unsupported and experimental" => Some("No compatible y experimental"),
        "Unsupported settings export version {version}; this build supports version {supported}." => {
            Some(
                "Versión de exportación de configuración no compatible {version}; esta compilación admite la versión {supported}.",
            )
        }
        "Until I turn it off" => Some("Hasta que lo desactive"),
        "Until tomorrow ({wake})" => Some("Hasta mañana ({wake})"),
        "Update password" => Some("Actualizar contraseña"),
        "Use WebKitGTK's local spell checker in the WhatsApp composer." => {
            Some("Usa el corrector ortográfico local de WebKitGTK en el editor de WhatsApp.")
        }
        "Use dark theme" => Some("Usar tema oscuro"),
        "Use default" => Some("Usar predeterminada"),
        "Use global" => Some("Usar configuración global"),
        "Use global ({value})" => Some("Usar global ({value})"),
        "Use light theme" => Some("Usar tema claro"),
        "Use modifiers plus one key, for example Ctrl+Shift+W." => {
            Some("Usa modificadores más una tecla, por ejemplo Ctrl+Shift+W.")
        }
        "Use the desktop network settings" => Some("Usar la configuración de red del escritorio"),
        "Username" => Some("Usuario"),
        "Version {version}" => Some("Versión {version}"),
        "Version {version} is available" => Some("La versión {version} está disponible"),
        "Version {version} is skipped." => Some("La versión {version} está omitida."),
        "View release" => Some("Ver versión"),
        "View-once messages" => Some("Mensajes de ver una vez"),
        "Waiting for the file dialog or the current operation…" => {
            Some("Esperando al diálogo de archivos o a la operación actual…")
        }
        "Waiting for the file dialog…" => Some("Esperando al diálogo de archivos…"),
        "Weekdays" => Some("Entre semana"),
        "Weekends" => Some("Fines de semana"),
        "Welcome to OxideZap" => Some("Te damos la bienvenida a OxideZap"),
        "What suspension keeps" => Some("Qué conserva la suspensión"),
        "WhatsApp Web changes its DOM without notice. A stylesheet can break the QR login, calls, dialogs, or the chat view, and may stop working after any WhatsApp update. Do not hide security, privacy, or abuse-reporting controls. OxideZap injects CSS only: it never runs JavaScript from this file and never downloads remote themes." => {
            Some(
                "WhatsApp Web cambia su DOM sin avisar. Una hoja de estilos puede romper el inicio de sesión por QR, las llamadas, los diálogos o la vista de chat, y puede dejar de funcionar tras cualquier actualización de WhatsApp. No ocultes controles de seguridad, privacidad ni denuncia de abuso. OxideZap solo inyecta CSS: nunca ejecuta JavaScript de este archivo ni descarga temas remotos.",
            )
        }
        "WhatsApp Web could not start" => Some("WhatsApp Web no pudo iniciarse"),
        "WhatsApp Web crashed" => Some("WhatsApp Web se cerró inesperadamente"),
        "WhatsApp Web for Linux" => Some("WhatsApp Web para Linux"),
        "WhatsApp Web is not running, so its memory was released. Your login, chats, and settings stay on disk." => {
            Some(
                "WhatsApp Web no se está ejecutando, así que se liberó su memoria. Tu inicio de sesión, chats y ajustes permanecen en disco.",
            )
        }
        "WhatsApp Web is offline" => Some("WhatsApp Web está desconectado"),
        "WhatsApp Web is offline." => Some("WhatsApp Web está desconectado."),
        "WhatsApp Web notifications are delivered by the desktop notification service." => Some(
            "Las notificaciones de WhatsApp Web las entrega el servicio de notificaciones del escritorio.",
        ),
        "WhatsApp Web zoom is limited to {min}%–{max}%. It does not resize the native UI; use Accessibility for native text size." => {
            Some(
                "El zoom de WhatsApp Web está limitado a {min}%–{max}%. No cambia el tamaño de la interfaz nativa; usa Accesibilidad para el tamaño del texto nativo.",
            )
        }
        "WhatsApp keeps handling the account, the QR login, and your messages. OxideZap only provides the desktop window around it." => {
            Some(
                "WhatsApp sigue gestionando la cuenta, el inicio de sesión por QR y tus mensajes. OxideZap solo proporciona la ventana de escritorio que lo rodea.",
            )
        }
        "WhatsApp only lets you open view-once media on your phone. OxideZap does not bypass that restriction; the desktop window follows WhatsApp Web's own rules." => {
            Some(
                "WhatsApp solo permite abrir medios de ver una vez en tu teléfono. OxideZap no elude esa restricción; la ventana de escritorio sigue las propias reglas de WhatsApp Web.",
            )
        }
        "Where your data lives" => Some("Dónde viven tus datos"),
        "Window" => Some("Ventana"),
        "Window behavior, startup, writing, downloads, media actions, and the global show/hide shortcut." => {
            Some(
                "Comportamiento de ventanas, inicio, escritura, descargas, acciones de medios y el atajo global de mostrar/ocultar.",
            )
        }
        "Window open" => Some("Ventana abierta"),
        "Windows and privacy" => Some("Ventanas y privacidad"),
        "Wrap IPv6 addresses in brackets, for example [::1]." => {
            Some("Escribe las direcciones IPv6 entre corchetes, por ejemplo [::1].")
        }
        "Writing" => Some("Escritura"),
        "You are running the latest release." => Some("Estás usando la última versión."),
        "You're ready" => Some("Todo listo"),
        "Your session stays on this computer" => Some("Tu sesión permanece en este equipo"),
        "Zoom in" => Some("Acercar"),
        "Zoom out" => Some("Alejar"),
        "Zoom: {from}% -> {to}%" => Some("Zoom: {from}% -> {to}%"),
        "applied" => Some("aplicado"),
        "clear the cache" => Some("borrar la caché"),
        "delete the account data" => Some("eliminar los datos de la cuenta"),
        "every day" => Some("todos los días"),
        "less than 1m left" => Some("queda menos de 1 min"),
        "no active account; pass --account" => Some("no hay una cuenta activa; pasa --account"),
        "no address was returned" => Some("no se devolvió ninguna dirección"),
        "no days" => Some("ningún día"),
        "not applied" => Some("no aplicado"),
        "not configured" => Some("sin configurar"),
        "off" => Some("desactivado"),
        "on" => Some("activado"),
        "on until turned off" => Some("activado hasta que se desactive"),
        "quiet hours {schedule}" => Some("silencio {schedule}"),
        "release hidden accounts after {minutes} min" => {
            Some("liberar cuentas ocultas tras {minutes} min")
        }
        "sign out" => Some("cerrar sesión"),
        "suspended" => Some("suspendida"),
        "the HTTP proxy" => Some("el proxy HTTP"),
        "the HTTPS proxy" => Some("el proxy HTTPS"),
        "the SOCKS proxy" => Some("el proxy SOCKS"),
        "the application is locked; unlock it first" => {
            Some("la aplicación está bloqueada; desbloquéala primero")
        }
        "the application lock is not configured" => {
            Some("el bloqueo de la aplicación no está configurado")
        }
        "the account no longer exists" => Some("la cuenta ya no existe"),
        "the last account cannot be deleted" => Some("la última cuenta no se puede eliminar"),
        "Enter a shortcut." => Some("Introduce un atajo."),
        "Add a key to the shortcut." => Some("Añade una tecla al atajo."),
        "A global shortcut needs at least one modifier." => {
            Some("Un atajo global necesita al menos un modificador.")
        }
        "That key is not supported for global shortcuts." => {
            Some("Esa tecla no es compatible con los atajos globales.")
        }
        "tomorrow {time}" => Some("mañana {time}"),
        "weekdays" => Some("entre semana"),
        "weekends" => Some("fines de semana"),
        "{active} of {total}" => Some("{active} de {total}"),
        "{count} command" => Some("{count} comando"),
        "{count} commands" => Some("{count} comandos"),
        "{count} item" => Some("{count} elemento"),
        "{count} items" => Some("{count} elementos"),
        "{count} items · {active} in progress" => Some("{count} elementos · {active} en curso"),
        "{count} message" => Some("{count} mensaje"),
        "{count} messages" => Some("{count} mensajes"),
        "{count} suspended account" => Some("{count} cuenta suspendida"),
        "{count} suspended accounts" => Some("{count} cuentas suspendidas"),
        "{count} unread message" => Some("{count} mensaje sin leer"),
        "{count} unread messages" => Some("{count} mensajes sin leer"),
        "{days}d left" => Some("quedan {days} d"),
        "{days}d {hours}h left" => Some("quedan {days} d {hours} h"),
        "{filename}\nSaved to {destination}" => Some("{filename}\nGuardado en {destination}"),
        "{filename}\nSaving to {folder}" => Some("{filename}\nGuardando en {folder}"),
        "{filename}\n{hint}" => Some("{filename}\n{hint}"),
        "{filename}\n{reason}" => Some("{filename}\n{reason}"),
        "{hours} h ago" => Some("hace {hours} h"),
        "{hours}h left" => Some("quedan {hours} h"),
        "{hours}h {minutes}m left" => Some("quedan {hours} h {minutes} min"),
        "{minutes} min ago" => Some("hace {minutes} min"),
        "{minutes}m left" => Some("quedan {minutes} min"),
        "{name} is connected again." => Some("{name} vuelve a estar conectada."),
        "{name} is on the WhatsApp login page. OxideZap did not reload it automatically; use Retry now if the page is stuck." => {
            Some(
                "{name} está en la página de inicio de sesión de WhatsApp. OxideZap no la recargó automáticamente; usa Reintentar ahora si la página está atascada.",
            )
        }
        "{name} lost its connection. OxideZap will reconnect when the network returns." => {
            Some("{name} perdió su conexión. OxideZap se reconectará cuando vuelva la red.")
        }
        "{path} is not valid UTF-8." => Some("{path} no es UTF-8 válido."),
        "{percent}% · {received} of {total}" => Some("{percent}% · {received} de {total}"),
        "{protocol} proxy at {endpoint}" => Some("proxy {protocol} en {endpoint}"),
        "{result} Resolved from the desktop system proxy settings." => {
            Some("{result} Resuelto desde la configuración del proxy del sistema del escritorio.")
        }
        "{size} · {state}" => Some("{size} · {state}"),
        "{theme} · {zoom}%" => Some("{theme} · {zoom}%"),
        "↑/↓ Move · Enter Run · Esc Close" => Some("↑/↓ Mover · Enter Ejecutar · Esc Cerrar"),
        "Hidden" => Some("Oculto"),
        "Full preview" => Some("Vista previa completa"),
        "Hide previews when locked: {state}" => Some("Ocultar vistas previas al bloquear: {state}"),
        "Hide previews while OxideZap is locked" => {
            Some("Ocultar vistas previas mientras OxideZap está bloqueado")
        }
        "Message from {sender}" => Some("Mensaje de {sender}"),
        "New WhatsApp message" => Some("Nuevo mensaje de WhatsApp"),
        "Notification privacy" => Some("Privacidad de notificaciones"),
        "Notification privacy controls what the native notification shows. Unread counts, window titles, the tray badge, and the click target never change." => {
            Some(
                "La privacidad de notificaciones controla qué muestra la notificación nativa. Los contadores sin leer, los títulos de ventana, la insignia de la bandeja y el destino del clic nunca cambian.",
            )
        }
        "Notification privacy: {from} -> {to}" => {
            Some("Privacidad de notificaciones: {from} -> {to}")
        }
        "Open OxideZap for details." => Some("Abre OxideZap para ver los detalles."),
        "Open OxideZap to read it." => Some("Abre OxideZap para leerlo."),
        "Preview" => Some("Vista previa"),
        "See you at 18:00." => Some("Nos vemos a las 18:00."),
        "Sender only" => Some("Solo remitente"),
        "Sender only falls back to Hidden when WhatsApp does not provide a sender." => {
            Some("Solo remitente recurre a Oculto cuando WhatsApp no proporciona un remitente.")
        }
        "Shows a generic message with no sender or content." => {
            Some("Muestra un mensaje genérico sin remitente ni contenido.")
        }
        "Shows the sender and the message text." => {
            Some("Muestra el remitente y el texto del mensaje.")
        }
        "Shows the sender name but hides the message text." => {
            Some("Muestra el nombre del remitente pero oculta el texto del mensaje.")
        }
        "The notification daemon and the operating system may still store or show content on their own lock screen; OxideZap cannot control that." => {
            Some(
                "El demonio de notificaciones y el sistema operativo pueden seguir guardando o mostrando contenido en su propia pantalla de bloqueo; OxideZap no puede controlarlo.",
            )
        }
        "The selected level applies to message and system notifications such as downloads and errors. Connection and crash notifications keep their full text." => {
            Some(
                "El nivel seleccionado se aplica a los mensajes y a las notificaciones del sistema, como descargas y errores. Las notificaciones de conexión y de fallos conservan su texto completo.",
            )
        }
        "This is what a message notification looks like with the current selection." => {
            Some("Así se ve una notificación de mensaje con la selección actual.")
        }
        "While the app is locked, notifications use the Hidden level regardless of the selection above." => {
            Some(
                "Mientras la aplicación está bloqueada, las notificaciones usan el nivel Oculto independientemente de la selección anterior.",
            )
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_placeholders_mismatch(english: &str, spanish: &str) {
        let placeholders = |text: &str| {
            let mut names: Vec<String> = Vec::new();
            let mut rest = text;
            while let Some(start) = rest.find('{') {
                let after = &rest[start + 1..];
                let Some(end) = after.find('}') else {
                    break;
                };
                names.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            names.sort();
            names
        };
        assert_eq!(
            placeholders(english),
            placeholders(spanish),
            "placeholders differ for {english:?}"
        );
    }

    #[test]
    fn locale_parsing_accepts_encodings_countries_and_modifiers() {
        assert_eq!(parse_locale("es_AR.UTF-8"), Some(Language::Spanish));
        assert_eq!(parse_locale("es_ES@euro"), Some(Language::Spanish));
        assert_eq!(parse_locale("ES_es"), Some(Language::Spanish));
        assert_eq!(parse_locale("en_GB.utf8"), Some(Language::English));
        assert_eq!(parse_locale("en"), Some(Language::English));
        assert_eq!(parse_locale("es:en"), Some(Language::Spanish));
    }

    #[test]
    fn unknown_locales_fall_back_to_english() {
        assert_eq!(parse_locale("fr_FR.UTF-8"), None);
        assert_eq!(parse_locale("de"), None);
        assert_eq!(parse_locale("C"), None);
        assert_eq!(parse_locale("POSIX"), None);
        assert_eq!(parse_locale(""), None);
        assert_eq!(
            Language::resolved(Language::System, Some("pt_BR.UTF-8")),
            Language::English
        );
        assert_eq!(
            Language::resolved(Language::System, None),
            Language::English
        );
    }

    #[test]
    fn explicit_language_overrides_the_system_locale() {
        assert_eq!(
            Language::resolved(Language::English, Some("es_ES.UTF-8")),
            Language::English
        );
        assert_eq!(
            Language::resolved(Language::Spanish, Some("en_US.UTF-8")),
            Language::Spanish
        );
    }

    #[test]
    fn interpolation_replaces_known_placeholders_and_keeps_unknown_ones() {
        let args = [("count", "3".to_string()), ("name", "Work".to_string())];
        assert_eq!(
            interpolate("{count} messages for {name}", &args),
            "3 messages for Work"
        );
        assert_eq!(interpolate("Hello {missing}", &args), "Hello {missing}");
    }

    #[test]
    fn missing_keys_return_the_key_without_panicking() {
        let key = "a key that is not translated";
        assert_eq!(translate_for(Language::English, key), key);
        assert_eq!(translate_for(Language::Spanish, key), key);
    }

    #[test]
    fn plural_helper_uses_singular_for_exactly_one() {
        assert_eq!(tn!(1, "{count} message", "{count} messages"), "1 message");
        assert_eq!(tn!(2, "{count} message", "{count} messages"), "2 messages");
    }

    #[test]
    fn spanish_plural_forms_use_count_aware_translations() {
        assert_eq!(
            translate_for(Language::Spanish, "{count} unread message"),
            "{count} mensaje sin leer"
        );
        assert_eq!(
            translate_for(Language::Spanish, "{count} unread messages"),
            "{count} mensajes sin leer"
        );
        let args = [("count", "1".to_string())];
        assert_eq!(
            interpolate(
                translate_for(Language::Spanish, "{count} unread message"),
                &args
            ),
            "1 mensaje sin leer"
        );
    }

    #[test]
    fn spanish_table_has_complete_entries() {
        for (english, spanish) in SPANISH_KEYS {
            assert!(!english.is_empty(), "an empty English key");
            assert!(!spanish.is_empty(), "empty translation for {english:?}");
            assert_no_placeholders_mismatch(english, spanish);
        }
    }

    /// Scans the scoped sources for `t!`, `tr!`, and `tn!` keys and fails when
    /// any key is missing from the Spanish table. Test-only strings are not
    /// exempt: tests that render UI use the same keys.
    #[test]
    fn every_used_key_has_a_spanish_translation() {
        let sources: &[&str] = &[
            include_str!("i18n.rs"),
            include_str!("ui/account_window.rs"),
            include_str!("ui/command_palette.rs"),
            include_str!("ui/components.rs"),
            include_str!("ui/downloads_window.rs"),
            include_str!("ui/lock_window.rs"),
            include_str!("ui/onboarding_window.rs"),
            include_str!("ui/settings_window.rs"),
            include_str!("app.rs"),
            include_str!("main.rs"),
            include_str!("notify.rs"),
            include_str!("settings.rs"),
            include_str!("tray.rs"),
        ];

        let mut used: Vec<String> = DYNAMIC_KEYS.iter().map(|key| key.to_string()).collect();
        for source in sources {
            for key in scanned_keys(source) {
                if !used.contains(&key) {
                    used.push(key);
                }
            }
        }

        let mut missing: Vec<&str> = used
            .iter()
            .filter(|key| spanish_translation(key).is_none())
            .map(String::as_str)
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "missing Spanish translations for {} keys: {missing:#?}",
            missing.len()
        );

        let unused: Vec<&str> = SPANISH_KEYS
            .iter()
            .map(|(english, _)| *english)
            .filter(|key| !used.iter().any(|used| used == key))
            .collect();
        assert!(
            unused.is_empty(),
            "translation table has keys the UI no longer uses: {unused:#?}"
        );
    }

    fn scanned_keys(source: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let markers = [concat!("t", "!("), concat!("tr", "!("), concat!("tn", "!(")];
        for marker in markers {
            let mut search_from = 0;
            while let Some(relative) = source[search_from..].find(marker) {
                let index = search_from + relative;
                search_from = index + marker.len();
                if index > 0
                    && source[..index]
                        .chars()
                        .next_back()
                        .is_some_and(|character| {
                            character.is_ascii_alphanumeric() || character == '_'
                        })
                {
                    continue;
                }
                let after = source[index + marker.len()..].trim_start();
                let (arguments, _consumed) = macro_arguments(after);
                if marker.starts_with("tn") {
                    let mut parts = split_top_level(arguments);
                    if parts.len() >= 3 {
                        let one = parse_string_literal(parts.remove(1));
                        let many = parse_string_literal(parts.remove(1));
                        if let Some(one) = one {
                            keys.push(one);
                        }
                        if let Some(many) = many {
                            keys.push(many);
                        }
                    }
                } else if let Some(key) = parse_string_literal(arguments) {
                    keys.push(key);
                }
            }
        }
        keys
    }

    /// Returns the text between the macro's parentheses and how much of the
    /// input was consumed, including the closing parenthesis.
    fn macro_arguments(input: &str) -> (&str, usize) {
        let mut depth = 1usize;
        let mut in_string = false;
        let mut escaped = false;
        for (index, character) in input.char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
                continue;
            }
            match character {
                '"' => in_string = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return (&input[..index], index + 1);
                    }
                }
                _ => {}
            }
        }
        (input, input.len())
    }

    fn split_top_level(input: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut start = 0usize;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        for (index, character) in input.char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
                continue;
            }
            match character {
                '"' => in_string = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    parts.push(input[start..index].trim());
                    start = index + 1;
                }
                _ => {}
            }
        }
        parts.push(input[start..].trim());
        parts
    }

    fn parse_string_literal(input: &str) -> Option<String> {
        let input = input.trim_start();
        let mut characters = input.chars();
        if characters.next()? != '"' {
            return None;
        }
        let mut result = String::new();
        let mut escaped = false;
        while let Some(character) = characters.next() {
            if escaped {
                match character {
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    '0' => result.push('\0'),
                    '\\' => result.push('\\'),
                    '"' => result.push('"'),
                    'x' => {
                        let hex: String = characters.by_ref().take(2).collect();
                        let value = u8::from_str_radix(&hex, 16).ok()?;
                        result.push(value as char);
                    }
                    'u' => {
                        let rest: String = characters.clone().collect();
                        let rest = rest.strip_prefix('{')?;
                        let end = rest.find('}')?;
                        let value = u32::from_str_radix(&rest[..end], 16).ok()?;
                        result.push(char::from_u32(value)?);
                        for _ in 0..end + 2 {
                            characters.next();
                        }
                    }
                    '\n' => {
                        while let Some(next) = characters.clone().next() {
                            if next.is_whitespace() {
                                characters.next();
                            } else {
                                break;
                            }
                        }
                    }
                    other => result.push(other),
                }
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                return Some(result);
            } else {
                result.push(character);
            }
        }
        None
    }
}
