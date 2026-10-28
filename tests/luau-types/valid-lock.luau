--!strict

return ui.lock {
    on_key = function(event)
        local _key: string = event.key
        local _text: string? = event.text
        return false
    end,
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
                    autofocus = true,
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
