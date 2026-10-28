--!strict

return ui.lock {
    children = {
        ui.column {
            children = {
                ui.text { text = "locked" },
                ui.secret_input {
                    placeholder = "password",
                    on_hover = function(inside)
                        local _inside: boolean = inside
                    end,
                    style = {
                        width = 240,
                        caret_color = "#ffffff",
                    },
                },
            },
        },
    },
}
