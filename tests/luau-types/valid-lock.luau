--!strict

return ui.lock {
    children = {
        ui.column {
            children = {
                ui.text { text = "locked" },
                ui.secret_input {
                    placeholder = "password",
                    style = {
                        width = 240,
                        caret_color = "#ffffff",
                    },
                },
            },
        },
    },
}
