CREATE TABLE sales_questionnaires (
    id INT PRIMARY KEY AUTO_INCREMENT,
    name VARCHAR(255) NOT NULL,
    company_id INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
        ON UPDATE CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL,

    CONSTRAINT fk_sales_questionnaires_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE
);

CREATE TABLE sales_questionnaire_questions (
    id INT PRIMARY KEY AUTO_INCREMENT,
    questionnaire_id INT NOT NULL,
    question TEXT NOT NULL,
    answer TEXT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
        ON UPDATE CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL,

    CONSTRAINT fk_sales_questionnaire_questions_questionnaire
        FOREIGN KEY (questionnaire_id)
        REFERENCES sales_questionnaires(id)
        ON DELETE CASCADE
);

CREATE TABLE sales_questionnaire_responses (
    id INT PRIMARY KEY AUTO_INCREMENT,
    deal_id BIGINT UNSIGNED NOT NULL,
    question_id INT NOT NULL,
    answer TEXT NULL,
    answered_by INT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
        ON UPDATE CURRENT_TIMESTAMP,

    UNIQUE KEY uq_deal_question (deal_id, question_id),

    CONSTRAINT fk_questionnaire_response_deal
        FOREIGN KEY (deal_id)
        REFERENCES deals(id)
        ON DELETE CASCADE,

    CONSTRAINT fk_questionnaire_response_question
        FOREIGN KEY (question_id)
        REFERENCES sales_questionnaire_questions(id)
        ON DELETE CASCADE,

    CONSTRAINT fk_questionnaire_response_user
        FOREIGN KEY (answered_by)
        REFERENCES users(id)
        ON DELETE SET NULL
);