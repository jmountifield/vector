package metadata

installation: roles: agent: {
	title: "Agent"
	sub_roles: {
		daemon: {
			title: "Daemon"
			description: """
				The daemon role is designed to collect _all_ data on a single host. This is
				the recommended role for data collection since it the most efficient use
				of host resources. Vector implements a directed acyclic graph topology model,
				enabling the collection and processing from mutliple services.
				"""
		}
		sidecar: {
			title: "Sidecar"
			description: """
				The sidecar role couples Vector with each service, focused on data collection
				for that individual service only. While the deamon role is recommended, the
				sidecar role is beneficial when you want to shift reponsibility of data
				collection to the service owner. And, in some cases, it can be simpler to
				manage.
				"""
		}
	}
}
