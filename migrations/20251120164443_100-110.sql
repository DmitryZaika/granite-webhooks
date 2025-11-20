-- Add migration script here
ALTER TABLE checklists ADD COLUMN company_id INT NULL;

ALTER TABLE checklists ADD CONSTRAINT fk_checklists_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE;

-- ВАЖНО: Выполнять в InnoDB. Желательно в простое (могут быть блокировки).
-- Если в твоём runner-е транзакции для DDL не поддерживаются, оставь как есть — шаги упадут / пройдут по очереди.

-- 1) USERS: снять FK, чтобы можно было менять NULLability
ALTER TABLE users
    DROP FOREIGN KEY fk_company_users_id;

-- 2) USERS: привести company_id к NOT NULL (тип подгони под company.id)
ALTER TABLE users
    MODIFY COLUMN company_id INT NOT NULL;

-- 3) USERS: вернуть FK (имя оставляем прежним из лога, чтобы было прозрачно)
--    При необходимости добавь ON DELETE/UPDATE, если это твоя целевая политика.
ALTER TABLE users
    ADD CONSTRAINT fk_company_users_id
    FOREIGN KEY (company_id) REFERENCES company(id);

-- 4) USER_POSITIONS: добавить company_id, если ещё нет (nullable для безопасного бэкфила)
ALTER TABLE users_positions
    ADD COLUMN company_id INT;

-- 5) БЭКФИЛЛ из users -> users_positions
UPDATE users_positions up
JOIN users u ON u.id = up.user_id
SET up.company_id = u.company_id
WHERE up.company_id IS NULL;

-- 6) USER_POSITIONS: теперь можно сделать NOT NULL
ALTER TABLE users_positions
    MODIFY COLUMN company_id INT NOT NULL;

-- 7) USER_POSITIONS: вешаем целевой FK на company(id)
ALTER TABLE users_positions
    ADD CONSTRAINT fk_users_positions_company_id
    FOREIGN KEY (company_id) REFERENCES company(id);

CREATE TABLE emails (
    id INT AUTO_INCREMENT PRIMARY KEY,
    user_id INT NOT NULL,
    subject VARCHAR(255) NOT NULL,
    body TEXT NOT NULL,
    sent_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP DEFAULT NULL,
    message_id VARCHAR(72) NOT NULL UNIQUE,
    FOREIGN KEY (user_id) REFERENCES users(id)
);


CREATE TABLE email_reads (
    id INT AUTO_INCREMENT PRIMARY KEY,
    message_id VARCHAR(72) NOT NULL,
    read_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    user_agent VARCHAR(500),
    ip_address VARCHAR(100),
    FOREIGN KEY (message_id) REFERENCES emails(message_id)
);
