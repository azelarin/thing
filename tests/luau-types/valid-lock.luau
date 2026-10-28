--!strict

return ui.lock {
    children = {
        ui.column {
            children = {
                ui.text { text = "locked" },
                ui.text {
                    text = function()
                        local state: ThingAuthenticationState =
                            services.authentication.state:get()
                        return if state == "waiting"
                            then services.authentication.prompt:get()
                            else services.authentication.message:get()
                    end,
                },
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
