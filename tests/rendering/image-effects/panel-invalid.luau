-- valid luau whose blur fails runtime validation, so the host must keep the
-- previous surfaces.
return require('./gallery')({ blur = 65, saturation = 1 })
