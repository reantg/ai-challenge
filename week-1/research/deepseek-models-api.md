# DeepSeek API: получение списка моделей

## Вывод

**Да, меню `/model` можно строить динамически.** Официальный endpoint DeepSeek «Lists Models» возвращает список доступных в данный момент моделей, а поле `data[].id` прямо описано как идентификатор, который можно передавать в API. Для меню следует запрашивать endpoint с API-ключом и использовать значения `data[].id`, не полагаясь на список моделей из примера документации.

Источник: [Lists Models — DeepSeek API Docs](https://api-docs.deepseek.com/api/list-models).

## HTTP-запрос

- Метод и путь: `GET /models`.
- Базовый URL: `https://api.deepseek.com`.
- Полный URL: `https://api.deepseek.com/models`.
- Авторизация: `Authorization: Bearer <DEEPSEEK_API_KEY>`.
- Официальный сгенерированный пример запроса также передаёт `Accept: application/json`.
- Тело, query-параметры и path-параметры для этого endpoint документацией не предусмотрены; `Content-Type` в официальном примере не отправляется.

Минимальный запрос по образцу документации:

```http
GET /models HTTP/1.1
Host: api.deepseek.com
Accept: application/json
Authorization: Bearer <DEEPSEEK_API_KEY>
```

Источники: [endpoint и пример запроса](https://api-docs.deepseek.com/api/list-models), [базовый URL и общий пример Bearer-авторизации](https://api-docs.deepseek.com/).

## Успешный ответ

Статус: `200 OK`. Формат: `application/json`.

```json
{
  "object": "list",
  "data": [
    {
      "id": "string",
      "object": "model",
      "owned_by": "string"
    }
  ]
}
```

Все перечисленные поля обязательны:

| Поле | Тип / ограничение | Значение |
|---|---|---|
| `object` | `string`, только `"list"` | Тип корневого объекта |
| `data` | `Model[]` | Массив моделей |
| `data[].id` | `string` | Идентификатор модели для вызовов API |
| `data[].object` | `string`, только `"model"` | Тип объекта модели |
| `data[].owned_by` | `string` | Организация-владелец модели |

Источник: [схема ответа Lists Models](https://api-docs.deepseek.com/api/list-models).

## Документированные ошибки

Страница самого endpoint описывает только ответ `200`; отдельную JSON-схему тела ошибки для `/models` она не публикует. Общая официальная таблица ошибок DeepSeek API документирует следующие HTTP-статусы:

| HTTP | Название в документации | Причина / действие клиента |
|---:|---|---|
| `400` | Invalid Format | Некорректный формат тела запроса; исправить по сообщению ошибки |
| `401` | Authentication Fails | Неверный API-ключ; проверить ключ |
| `402` | Insufficient Balance | Недостаточно средств; проверить и пополнить баланс |
| `422` | Invalid Parameters | Некорректные параметры; исправить по сообщению ошибки |
| `429` | Rate Limit Reached | Запросы отправляются слишком быстро; снизить частоту |
| `500` | Server Error | Ошибка сервера; повторить после небольшой паузы, при повторении обратиться в поддержку |
| `503` | Server Overloaded | Сервер перегружен; повторить после небольшой паузы |

Источник: [Error Codes — DeepSeek API Docs](https://api-docs.deepseek.com/quick_start/error_codes).

## Практика для `/model`

1. Выполнить аутентифицированный `GET https://api.deepseek.com/models`.
2. При `200` проверить корневое `object === "list"` и прочитать массив `data`.
3. Для значения команды/кнопки использовать `data[].id`; `owned_by` можно показывать как дополнительную информацию.
4. Не выводить модели из примера ответа как статический перечень: назначение endpoint — сообщать доступный в данный момент список.
5. Не предполагать недокументированные поля, порядок моделей или сведения об их возможностях: опубликованная схема гарантирует только `id`, `object` и `owned_by`.
6. Не разбирать текст ошибок как стабильный контракт: официальный справочник задаёт категории по HTTP-статусам, но не фиксирует JSON-схему ошибки для `/models`.

## Официальные URL

- https://api-docs.deepseek.com/api/list-models
- https://api-docs.deepseek.com/quick_start/error_codes
- https://api-docs.deepseek.com/
- https://platform.deepseek.com/api_keys
