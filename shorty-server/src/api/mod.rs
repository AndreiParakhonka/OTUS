//! HTTP-слой сервиса: DTO, единый тип ошибок, handlers.
//!
//! Дисциплина слоёв: axum-типы живут только здесь; `crates/domain`
//! не знает про HTTP. Конвертация на границе — `From`/`TryFrom`.

pub mod dto;
pub mod error;
pub mod handlers;
