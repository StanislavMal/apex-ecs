//! `ScriptableRegistrar` — трейт, реализуемый через `#[derive(Scriptable)]`.
//!
//! Обеспечивает двустороннее преобразование компонента в/из `mlua::Value`
//! и регистрацию конструктора в Lua.
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
//!     fn to_lua<'lua>(&self, lua: &'lua mlua::Lua) -> mlua::Result<mlua::Value<'lua>> {
//!         let t = lua.create_table()?;
//!         t.set("current", self.current)?;
//!         t.set("max", self.max)?;
//!         Ok(mlua::Value::Table(t))
//!     }
//!
//!     fn from_lua(val: &mlua::Value) -> Option<Self> {
//!         let t = val.as_table()?;
//!         let current = t.get::<f32>("current").ok()?;
//!         let max     = t.get::<f32>("max").ok()?;
//!         Some(Self { current, max })
//!     }
//!
//!     fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()> {
//!         let t = lua.create_table()?;
//!         t.set("new", lua.create_function(|lua, (current, max): (f32, f32)| {
//!             let t = lua.create_table()?;
//!             t.set("current", current)?;
//!             t.set("max", max)?;
//!             Ok(t)
//!         })?)?;
//!         lua.globals().set("Health", t)
//!     }
//! }
//! ```

/// Трейт для компонентов, доступных из Lua-скриптов.
///
/// Генерируется автоматически через `#[derive(Scriptable)]`.
/// Можно реализовать вручную для нестандартных типов.
pub trait ScriptableRegistrar: Sized + 'static {
    /// Строковое имя типа — используется как ключ в таблице внутри query-итератора.
    fn type_name_str() -> &'static str;

    /// Имена полей структуры — для документации и отладки.
    fn field_names() -> &'static [&'static str];

    /// Конвертировать значение компонента в mlua::Value (обычно Table).
    fn to_lua(&self, lua: &mlua::Lua) -> mlua::Result<mlua::Value>;

    /// Восстановить компонент из mlua::Value.
    ///
    /// Возвращает `None` если Value не является Table или поля отсутствуют/имеют
    /// неверный тип. Это штатная ситуация при работе со скриптами.
    fn from_lua(val: &mlua::Value) -> Option<Self>;

    /// Зарегистрировать конструктор типа в глобалах Lua.
    ///
    /// Например: `Position.new(x, y)`, `TileKind.Floor = 0`
    ///
    /// Вызывается один раз при `ScriptEngine::register_component::<T>()`.
    fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()>;
}

// ── ResourceBinding ─────────────────────────────────────────────

/// Информация о ресурсе, зарегистрированном для доступа из Lua-скриптов.
///
/// Аналогичен `ComponentBinding`, но для глобальных ресурсов (`World.resources`).
#[derive(Clone)]
pub struct ResourceBinding {
    /// Строковое имя типа ресурса.
    pub name: &'static str,
    /// Прочитать ресурс из `&World` → mlua::Value.
    /// Принимает Lua чтобы создавать таблицы.
    pub read:   fn(&mlua::Lua, &apex_core::World) -> mlua::Result<mlua::Value>,
    /// Записать ресурс в `&mut World` из mlua::Value.
    /// Возвращает `false` если тип неверен.
    pub write:  fn(&mlua::Value, &mut apex_core::World) -> bool,
}

// ── EventBinding ────────────────────────────────────────────────

/// Информация о событии, зарегистрированном для отправки из Lua-скриптов.
#[derive(Clone)]
pub struct EventBinding {
    /// Строковое имя типа события.
    pub name: &'static str,
    /// Отправить событие в `&mut World` (принимает mlua::Value, конвертирует в T).
    /// Возвращает `false` если событие не зарегистрировано или тип неверен.
    pub emit: fn(&mlua::Value, &mut apex_core::World) -> bool,
}
