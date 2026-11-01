--!strict

local track = ui.state(0)

return ui.panel {
    children = {
        ui.image {
            source = function(): string?
                local url = services.media.art_url:get()
                return if url ~= "" then url else nil
            end,
            revision = function(): string
                return services.media.title:get()
            end,
            fit = "cover",
        },
        ui.image {
            source = "/tmp/generated.png",
            revision = function(): number
                return track:get()
            end,
        },
        ui.button {
            on_click = function()
                track:set(track:get() + 1)
            end,
        },
    },
}
