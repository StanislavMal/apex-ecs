//! `ScriptableRegistrar` — трейт, реализуемый через `#[derive(Scriptable)]`.
//!
//! Обеспечивает двустороннее преобразование компонента в/из `rhai::Dynamic`
//! и регистрацию конструктора в Rhai Engine.
//!
//! Также содержит `ResourceBinding` — для доступа к глобальным ресурсам из скриптов.
//!
//! # Ручная реализация
//!
//! ```ignore
//! struct Health { current: f32, max: f32 }
//!
//! impl ScriptableRegistrar for Health {
//!     fn type_name_str() -> &'static str { "Health" }
//!
//!     fn field_names() -> &'static [&'static str] { &["current", "max"] }
//!
//!     fn to_dynamic(&self) -> Dynamic {
//!         let mut map = rhai::Map::new();
//!         map.insert("current".into(), Dynamic::from_float(self.current as f64));
//!         map.insert("max".into(),     Dynamic::from_float(self.max as f64));
//!         Dynamic::from_map(map)
//!     }
//!
//!     fn from_dynamic(d: &Dynamic) -> Option<Self> {
//!         let lock = d.read_lock::<rhai::Map>()?;
//!         let current = lock.get("current")?.as_float().ok()? as f32;
//!         let max     = lock.get("max")?.as_float().ok()? as f32;
//!         Some(Self { current, max })
//!     }
//!
//!     fn register_rhai_type(engine: &mut Engine) {
//!         engine.register_fn("Health", |current: f64, max: f64| -> Dynamic {
//!             let mut map = rhai::Map::new();
//!             map.insert("current".into(), Dynamic::from_float(current));
//!             map.insert("max".into(),     Dynamic::from_float(max));
//!             Dynamic::from_map(map)
//!         });
//!     }
//! }
//! ```

use rhai::{Dynamic, Engine};

/// Трейт для компонентов, доступных из Rhai-скриптов.
///
/// Генерируется автоматически через `#[derive(Scriptable)]`.
/// Можно реализовать вручную для нестандартных типов.
pub trait ScriptableRegistrar: Sized + 'static {
    /// Строковое имя типа — используется как ключ в Map внутри query-итератора.
    fn type_name_str() -> &'static str;

    /// Имена полей структуры — для документации и отладки.
    fn field_names() -> &'static [&'static str];

    /// Конвертировать значение компонента в Rhai Dynamic Map.
    ///
    /// Результат — `rhai::Map` с ключами = именам полей.
    fn to_dynamic(&self) -> Dynamic;

    /// Восстановить компонент из Rhai Dynamic.
    ///
    /// Возвращает `None` если Dynamic не является Map или поля отсутствуют/имеют
    /// неверный тип. Это штатная ситуация при работе со скриптами.
    fn from_dynamic(d: &Dynamic) -> Option<Self>;

    /// Зарегистрировать конструктор в Rhai Engine.
    ///
    /// Регистрирует функцию с именем типа (например, `Position(x, y)`)
    /// которая возвращает Dynamic Map с полями компонента.
    ///
    /// Вызывается один раз при `ScriptEngine::register_component::<T>()`.
    fn register_rhai_type(engine: &mut Engine);
}

// ── ResourceBinding ─────────────────────────────────────────────

/// Информация о ресурсе, зарегистрированном для доступа из Rhai-скриптов.
///
/// Аналогичен `ComponentBinding`, но для глобальных ресурсов (`World.resources`).
pub struct ResourceBinding {
    /// Строковое имя типа ресурса.
    pub name: &'static str,
    /// Прочитать ресурс из `&World` → Dynamic.
    /// Возвращает `None` если ресурс не найден.
    pub read:   fn(&apex_core::World) -> Option<Dynamic>,
    /// Записать ресурс в `&mut World` из Dynamic.
    /// Возвращает `false` если тип неверен.
    pub write:  fn(&mut apex_core::World, &Dynamic) -> bool,
}

// ── EventBinding ────────────────────────────────────────────────

/// Информация о событии, зарегистрированном для отправки из Rhai-скриптов.
pub struct EventBinding {
    /// Строковое имя типа события.
    pub name: &'static str,
    /// Отправить событие в `&mut World` (принимает Dynamic, конвертирует в T).
    /// Возвращает `false` если событие не зарегистрировано или тип неверен.
    pub emit: fn(&mut apex_core::World, &Dynamic) -> bool,
}